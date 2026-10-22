//! Validation tests for the idempotent side-effect dispatcher (Issue #220).
//!
//! These tests are the issue's validation artifact. They cover:
//!
//! - keyed planning (`dispatch`) and its idempotency across retries,
//! - exactly-once application, with replay and re-dispatch as no-ops,
//! - retry with exponential backoff and dead-lettering with forensic context,
//! - terminal (non-retryable) errors dead-lettering immediately,
//! - admin-gated dead-letter recovery, discard, and re-dispatch semantics,
//! - an end-to-end vertical slice: a settlement entry point that plans a
//!   referral side effect, retries it, and applies it exactly once.

extern crate std;

use std::cell::Cell;

use soroban_sdk::testutils::{Address as _, Events, Ledger};
use soroban_sdk::{
    contract, contractimpl, contracttype, Address, Bytes, BytesN, Env, Symbol, TryFromVal,
};

use crate::auth::set_admin;
use crate::dispatcher::{
    discard_effect, dispatch, effect, effect_dead_letters, effect_job, effect_key,
    effect_key_bytes, effect_key_pair, effect_stats, recover_effect, replay_effect, run_due_effect,
    DispatchOutcome, EffectHandler, EffectKind, EffectRecord, EffectSpec, FX_APPLIED, FX_DISCARDED,
    FX_PLANNED, FX_REPLAY_SKIPPED, FX_REQUEUED, FX_TOPIC, MAX_EFFECT_PAYLOAD_BYTES,
};
use crate::errors::Error;
use crate::jobs::{
    dead_letter_job_ids, get_receipt, pause_worker, resume_worker, JobError, JobStatus,
    RetryPolicy, RunOutcome,
};
use crate::storage::{instance_get, instance_set, persistent_get, persistent_has, persistent_set};

// ---------------------------------------------------------------------------
// Test-side effect state
// ---------------------------------------------------------------------------

/// Storage used by the test handlers to emulate user-visible effects.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
enum TestKey {
    /// How many times the user-visible action ran for an effect key.
    Credits(BytesN<32>),
    /// Count of blind (unguarded) writes, used by the hazard test.
    BlindWrites(BytesN<32>),
}

fn credit_count(env: &Env, key: &BytesN<32>) -> u32 {
    persistent_get(env, &TestKey::Credits(key.clone())).unwrap_or(0)
}

/// The correct handler shape: read-check-write, keyed by the effect key.
fn credit_once(env: &Env, key: &BytesN<32>) {
    if credit_count(env, key) == 0 {
        persistent_set(env, &TestKey::Credits(key.clone()), &1u32);
    }
}

/// Handler whose outcome is scripted: the first `remaining_failures` calls
/// fail with `error`, later calls apply the effect.
struct ScriptedHandler {
    calls: Cell<u32>,
    remaining_failures: Cell<u32>,
    error: Error,
}

impl ScriptedHandler {
    fn succeeding() -> Self {
        ScriptedHandler {
            calls: Cell::new(0),
            remaining_failures: Cell::new(0),
            error: Error::StaleData,
        }
    }

    fn failing(times: u32, error: Error) -> Self {
        ScriptedHandler {
            calls: Cell::new(0),
            remaining_failures: Cell::new(times),
            error,
        }
    }

    fn calls(&self) -> u32 {
        self.calls.get()
    }
}

impl EffectHandler for ScriptedHandler {
    fn apply(&self, env: &Env, effect: &EffectRecord) -> Result<(), Error> {
        self.calls.set(self.calls.get() + 1);
        if self.remaining_failures.get() > 0 {
            self.remaining_failures
                .set(self.remaining_failures.get() - 1);
            return Err(self.error);
        }
        credit_once(env, &effect.key);
        Ok(())
    }
}

/// A deliberately wrong handler: it mutates before it can fail. The dispatcher
/// retries it (correctly — it never saw a success), so the guard must live in
/// the handler.
struct BlindHandler {
    remaining_failures: Cell<u32>,
}

impl EffectHandler for BlindHandler {
    fn apply(&self, env: &Env, effect: &EffectRecord) -> Result<(), Error> {
        let writes = persistent_get(env, &TestKey::BlindWrites(effect.key.clone())).unwrap_or(0u32);
        persistent_set(
            env,
            &TestKey::BlindWrites(effect.key.clone()),
            &(writes + 1),
        );
        if self.remaining_failures.get() > 0 {
            self.remaining_failures
                .set(self.remaining_failures.get() - 1);
            return Err(Error::StaleData);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Fixture contract: the vertical slice
// ---------------------------------------------------------------------------

const AID_REFERRAL_TAG: u32 = 0x7265_6665; // "refe"

/// Fixture storage for the settlement slice.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AidKey {
    /// aid id -> settled amount.
    Settled(u64),
    /// Scripted handler failures left (dependency outage simulation).
    Failures,
}

/// Handler wired into the fixture contract: accrues one referral credit per
/// effect, and fails with a retryable error while a simulated outage lasts.
pub struct SettlementEffectHandler;

impl EffectHandler for SettlementEffectHandler {
    fn apply(&self, env: &Env, effect: &EffectRecord) -> Result<(), Error> {
        let remaining: u32 = instance_get(env, &AidKey::Failures).unwrap_or(0);
        if remaining > 0 {
            instance_set(env, &AidKey::Failures, &(remaining - 1));
            return Err(Error::StaleData);
        }
        credit_once(env, &effect.key);
        Ok(())
    }
}

#[contract]
pub struct AidSettlementFixture;

#[contractimpl]
impl AidSettlementFixture {
    /// The triggering action: record the settlement and plan its keyed
    /// referral follow-up in the same invocation. Re-settling the same aid is
    /// idempotent at both layers (settlement record and effect plan).
    pub fn settle_aid(env: Env, aid_id: u64, amount: i128) -> Result<DispatchOutcome, JobError> {
        if !persistent_has(&env, &AidKey::Settled(aid_id)) {
            persistent_set(&env, &AidKey::Settled(aid_id), &amount);
        }
        let mut payload = Bytes::new(&env);
        payload.append(&Bytes::from_slice(&env, &amount.to_be_bytes()));
        let spec = EffectSpec {
            kind: EffectKind::ReferralAccrual,
            key: effect_key(&env, AID_REFERRAL_TAG, aid_id),
            payload,
            source: aid_id,
        };
        dispatch(&env, spec, env.ledger().sequence(), None)
    }

    /// Simulate a dependency outage for the next `n` attempts.
    pub fn set_failures(env: Env, n: u32) {
        instance_set(&env, &AidKey::Failures, &n);
    }

    /// Permissionless crank, as an aid contract would expose it.
    pub fn run_effects(env: Env, worker: Address, now: u32) -> Result<RunOutcome, JobError> {
        run_due_effect(&env, &worker, now, &SettlementEffectHandler)
    }

    /// Deliberate replay of one aid's referral effect.
    pub fn replay_aid(
        env: Env,
        worker: Address,
        aid_id: u64,
        now: u32,
    ) -> Result<RunOutcome, JobError> {
        let key = effect_key(&env, AID_REFERRAL_TAG, aid_id);
        replay_effect(&env, &worker, &key, now, &SettlementEffectHandler)
    }

    pub fn effect_record(env: Env, aid_id: u64) -> Option<EffectRecord> {
        effect(&env, &effect_key(&env, AID_REFERRAL_TAG, aid_id))
    }

    /// The user-visible action's own counter: must never exceed 1 per aid.
    pub fn sent_referral_count(env: Env, aid_id: u64) -> u32 {
        credit_count(&env, &effect_key(&env, AID_REFERRAL_TAG, aid_id))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn set_ledger(env: &Env, sequence: u32) {
    env.ledger().set_sequence_number(sequence);
}

/// Registers the fixture and sets an admin. Returns (contract, admin).
fn setup(env: &Env) -> (Address, Address) {
    env.mock_all_auths();
    let admin = Address::generate(env);
    let contract_id = env.register_contract(None, AidSettlementFixture);
    env.as_contract(&contract_id, || {
        set_admin(env, &admin);
    });
    (contract_id, admin)
}

fn effect_spec(env: &Env, tag: u32, id: u64) -> EffectSpec {
    EffectSpec {
        kind: EffectKind::ReferralAccrual,
        key: effect_key(env, tag, id),
        payload: Bytes::from_slice(env, b"accrue"),
        source: id,
    }
}

fn policy(max_attempts: u32, base: u32, cap: u32) -> RetryPolicy {
    RetryPolicy {
        max_attempts,
        base_backoff_ledgers: base,
        max_backoff_ledgers: cap,
    }
}

// ---------------------------------------------------------------------------
// Keying and planning
// ---------------------------------------------------------------------------

#[test]
fn effect_keys_are_deterministic_and_structurally_distinct() {
    let env = Env::default();

    assert_eq!(effect_key(&env, 1, 5), effect_key(&env, 1, 5));
    assert_ne!(effect_key(&env, 1, 5), effect_key(&env, 1, 6));
    assert_ne!(effect_key(&env, 1, 5), effect_key(&env, 2, 5));

    assert_eq!(
        effect_key_pair(&env, 3, 1, 2),
        effect_key_pair(&env, 3, 1, 2)
    );
    assert_ne!(
        effect_key_pair(&env, 3, 1, 2),
        effect_key_pair(&env, 3, 2, 1)
    );

    let first = Bytes::from_slice(&env, b"aid:1/referrer:2");
    let second = Bytes::from_slice(&env, b"aid:12/referrer:2");
    assert_eq!(
        effect_key_bytes(&env, 9, &first),
        effect_key_bytes(&env, 9, &first)
    );
    assert_ne!(
        effect_key_bytes(&env, 9, &first),
        effect_key_bytes(&env, 9, &second)
    );
}

#[test]
fn planning_is_keyed_and_idempotent() {
    let env = Env::default();
    let (contract_id, _admin) = setup(&env);

    env.as_contract(&contract_id, || {
        let spec = effect_spec(&env, 1, 7);

        assert_eq!(
            dispatch(&env, spec.clone(), 0, None).unwrap(),
            DispatchOutcome::Planned(1)
        );
        // The same key is never planned twice, even when unrelated fields of
        // the request differ.
        assert_eq!(
            dispatch(&env, spec.clone(), 0, None).unwrap(),
            DispatchOutcome::AlreadyPending(1)
        );
        let mut resent = spec.clone();
        resent.source = 999;
        resent.payload = Bytes::from_slice(&env, b"different");
        assert_eq!(
            dispatch(&env, resent, 0, None).unwrap(),
            DispatchOutcome::AlreadyPending(1)
        );

        let stats = effect_stats(&env);
        assert_eq!(stats.counters.planned_total, 1);
        assert_eq!(stats.pending, 1);
        assert_eq!(stats.dead_letters, 0);

        let record = effect(&env, &spec.key).unwrap();
        assert_eq!(record.job_id, 1);
        assert_eq!(record.kind, EffectKind::ReferralAccrual);
        assert_eq!(record.source, 7);
        assert_eq!(record.applied_ledger, None);
        assert!(!record.discarded);

        // The backing job carries the retry state and links back by key.
        let job = effect_job(&env, &spec.key).unwrap();
        assert_eq!(job.status, JobStatus::Pending);
        assert_eq!(job.attempts, 0);
        assert_eq!(job.dedupe_key, spec.key);
    });
}

#[test]
fn oversized_payloads_are_rejected() {
    let env = Env::default();
    let (contract_id, _admin) = setup(&env);

    env.as_contract(&contract_id, || {
        let mut spec = effect_spec(&env, 2, 1);
        let mut payload = Bytes::new(&env);
        for _ in 0..(MAX_EFFECT_PAYLOAD_BYTES + 1) {
            payload.push_back(0u8);
        }
        spec.payload = payload;
        assert_eq!(dispatch(&env, spec, 0, None), Err(JobError::InvalidPayload));
    });
}

// ---------------------------------------------------------------------------
// Application and replay
// ---------------------------------------------------------------------------

#[test]
fn effect_applies_once_and_replay_is_a_noop() {
    let env = Env::default();
    let (contract_id, _admin) = setup(&env);
    let handler = ScriptedHandler::succeeding();
    let worker = Address::generate(&env);

    env.as_contract(&contract_id, || {
        set_ledger(&env, 5);
        let spec = effect_spec(&env, 3, 11);
        assert_eq!(
            dispatch(&env, spec.clone(), 5, None).unwrap(),
            DispatchOutcome::Planned(1)
        );

        let RunOutcome::Succeeded(job) = run_due_effect(&env, &worker, 5, &handler).unwrap() else {
            panic!("expected the effect to apply");
        };
        assert_eq!(job.attempts, 1);
        assert_eq!(job.status, JobStatus::Succeeded);

        let record = effect(&env, &spec.key).unwrap();
        assert_eq!(record.applied_ledger, Some(5));
        assert_eq!(credit_count(&env, &spec.key), 1, "applied exactly once");
        assert_eq!(effect_stats(&env).counters.applied_total, 1);
        assert_eq!(effect_stats(&env).pending, 0);

        // Nothing is due any more.
        assert_eq!(
            run_due_effect(&env, &worker, 6, &handler).unwrap(),
            RunOutcome::Empty
        );

        // Re-dispatching the same logical effect never re-plans it.
        assert_eq!(
            dispatch(&env, spec.clone(), 0, None).unwrap(),
            DispatchOutcome::AlreadyApplied(1)
        );

        // A replay returns the receipt without invoking the handler again.
        let receipt = get_receipt(&env, &spec.key).unwrap();
        assert_eq!(
            replay_effect(&env, &worker, &spec.key, 7, &handler).unwrap(),
            RunOutcome::AlreadyCompleted(receipt)
        );
        assert_eq!(handler.calls(), 1, "handler must not run twice");
        assert_eq!(effect_stats(&env).counters.replay_skips_total, 1);
        assert_eq!(credit_count(&env, &spec.key), 1);
    });
}

// ---------------------------------------------------------------------------
// Retry and dead-letter
// ---------------------------------------------------------------------------

#[test]
fn failed_attempts_retry_with_backoff_then_dead_letter_with_context() {
    let env = Env::default();
    let (contract_id, _admin) = setup(&env);
    let handler = ScriptedHandler::failing(u32::MAX, Error::StaleData);
    let worker = Address::generate(&env);

    env.as_contract(&contract_id, || {
        let spec = effect_spec(&env, 4, 21);
        assert_eq!(
            dispatch(&env, spec.clone(), 0, Some(policy(3, 10, 40))).unwrap(),
            DispatchOutcome::Planned(1)
        );

        set_ledger(&env, 0);
        let RunOutcome::Retried(job) = run_due_effect(&env, &worker, 0, &handler).unwrap() else {
            panic!("expected a retry");
        };
        assert_eq!(job.attempts, 1);
        assert_eq!(job.last_error, Error::StaleData as u32);
        assert_eq!(job.next_attempt_ledger, 10, "now + base backoff");

        // Not due before its backoff elapsed.
        assert_eq!(
            run_due_effect(&env, &worker, 9, &handler).unwrap(),
            RunOutcome::Empty
        );
        assert_eq!(handler.calls(), 1);

        set_ledger(&env, 10);
        let RunOutcome::Retried(job) = run_due_effect(&env, &worker, 10, &handler).unwrap() else {
            panic!("expected a retry");
        };
        assert_eq!(job.attempts, 2);
        assert_eq!(job.next_attempt_ledger, 30, "backoff doubled");

        set_ledger(&env, 30);
        let RunOutcome::DeadLettered(job) = run_due_effect(&env, &worker, 30, &handler).unwrap()
        else {
            panic!("expected a dead letter");
        };
        assert_eq!(job.attempts, 3);
        assert_eq!(job.status, JobStatus::DeadLettered);
        assert_eq!(handler.calls(), 3);

        // The dead letter is inspectable with full failure context…
        let dead_letters = effect_dead_letters(&env, None, 10);
        assert_eq!(dead_letters.len(), 1);
        let record = dead_letters.get(0).unwrap();
        assert_eq!(record.job_id, 1);
        assert_eq!(record.kind, crate::jobs::JobKind::Effect);
        assert_eq!(record.attempts, 3);
        assert_eq!(record.last_error, Error::StaleData as u32);
        assert_eq!(record.dedupe_key, spec.key);

        // …the queue exposes the failure…
        let stats = effect_stats(&env);
        assert_eq!(stats.dead_letters, 1);
        assert_eq!(stats.pending, 0);
        assert_eq!(stats.counters.applied_total, 0);

        // …and nothing else runs automatically: recovery is explicit.
        assert_eq!(
            run_due_effect(&env, &worker, 1_000, &handler).unwrap(),
            RunOutcome::Empty
        );
        assert_eq!(handler.calls(), 3);
        assert_eq!(credit_count(&env, &spec.key), 0);
    });
}

#[test]
fn terminal_errors_dead_letter_immediately() {
    let env = Env::default();
    let (contract_id, _admin) = setup(&env);
    let handler = ScriptedHandler::failing(u32::MAX, Error::Unauthorized);
    let worker = Address::generate(&env);

    env.as_contract(&contract_id, || {
        let spec = effect_spec(&env, 5, 1);
        dispatch(&env, spec.clone(), 0, Some(policy(5, 10, 40))).unwrap();

        let RunOutcome::DeadLettered(job) = run_due_effect(&env, &worker, 0, &handler).unwrap()
        else {
            panic!("a terminal error must dead-letter on the first attempt");
        };
        assert_eq!(job.attempts, 1);
        assert_eq!(job.last_error, Error::Unauthorized as u32);
        assert_eq!(handler.calls(), 1);
        assert_eq!(effect_stats(&env).dead_letters, 1);
    });
}

// ---------------------------------------------------------------------------
// Dead-letter recovery
// ---------------------------------------------------------------------------

#[test]
fn recovery_requeues_and_applies_exactly_once() {
    let env = Env::default();
    let (contract_id, admin) = setup(&env);
    let failing = ScriptedHandler::failing(u32::MAX, Error::StaleData);
    let succeeding = ScriptedHandler::succeeding();
    let worker = Address::generate(&env);
    let bystander = Address::generate(&env);

    env.as_contract(&contract_id, || {
        let spec = effect_spec(&env, 6, 33);
        dispatch(&env, spec.clone(), 0, Some(policy(2, 10, 40))).unwrap();

        set_ledger(&env, 0);
        assert!(matches!(
            run_due_effect(&env, &worker, 0, &failing).unwrap(),
            RunOutcome::Retried(_)
        ));
        set_ledger(&env, 10);
        assert!(matches!(
            run_due_effect(&env, &worker, 10, &failing).unwrap(),
            RunOutcome::DeadLettered(_)
        ));

        // A dead letter must be recovered before it can be replayed…
        assert_eq!(
            replay_effect(&env, &worker, &spec.key, 20, &succeeding),
            Err(JobError::NotRetryable)
        );
        // …and only an admin may recover it.
        assert_eq!(
            recover_effect(&env, &bystander, 1, 20),
            Err(JobError::Unauthorized)
        );

        set_ledger(&env, 20);
        let record = recover_effect(&env, &admin, 1, 20).unwrap();
        assert_eq!(record.job_id, 1);
        let job = effect_job(&env, &spec.key).unwrap();
        assert_eq!(job.status, JobStatus::Pending);
        assert_eq!(job.attempts, 0, "fresh attempt budget");
        assert_eq!(job.next_attempt_ledger, 20);

        // The replay now applies exactly once, even though earlier attempts
        // failed after being recorded in the dead-letter history.
        let RunOutcome::Succeeded(job) =
            replay_effect(&env, &worker, &spec.key, 20, &succeeding).unwrap()
        else {
            panic!("expected the recovered effect to apply");
        };
        assert_eq!(job.attempts, 1);
        assert_eq!(credit_count(&env, &spec.key), 1);
        assert_eq!(effect(&env, &spec.key).unwrap().applied_ledger, Some(20));
        assert_eq!(effect_stats(&env).counters.applied_total, 1);
        assert_eq!(effect_stats(&env).dead_letters, 0);

        // After recovery has applied, replay is a no-op again.
        assert!(matches!(
            replay_effect(&env, &worker, &spec.key, 21, &succeeding).unwrap(),
            RunOutcome::AlreadyCompleted(_)
        ));
        assert_eq!(credit_count(&env, &spec.key), 1);
    });
}

#[test]
fn discarded_dead_letters_keep_their_key_occupied() {
    let env = Env::default();
    let (contract_id, admin) = setup(&env);
    let handler = ScriptedHandler::failing(u32::MAX, Error::Unauthorized);
    let worker = Address::generate(&env);
    let bystander = Address::generate(&env);

    let key = effect_key(&env, 7, 1);
    env.as_contract(&contract_id, || {
        let spec = effect_spec(&env, 7, 1);
        dispatch(&env, spec.clone(), 0, None).unwrap();
        assert!(matches!(
            run_due_effect(&env, &worker, 0, &handler).unwrap(),
            RunOutcome::DeadLettered(_)
        ));
    });

    // Each `require_auth` for the same address needs its own contract frame.
    env.as_contract(&contract_id, || {
        assert_eq!(
            discard_effect(&env, &bystander, 1),
            Err(JobError::Unauthorized)
        );
    });
    env.as_contract(&contract_id, || {
        let record = discard_effect(&env, &admin, 1).unwrap();
        assert!(record.discarded);
        assert_eq!(dead_letter_job_ids(&env).len(), 0);
        assert_eq!(effect_stats(&env).dead_letters, 0);
    });

    env.as_contract(&contract_id, || {
        // The key stays occupied: re-running the triggering action reports the
        // discard instead of planning the unwanted effect again.
        assert_eq!(
            dispatch(&env, effect_spec(&env, 7, 1), 0, None).unwrap(),
            DispatchOutcome::Discarded(1)
        );
        assert_eq!(effect_stats(&env).counters.planned_total, 1);
    });

    // Recovery is still possible and clears the discard flag.
    env.as_contract(&contract_id, || {
        let recovered = recover_effect(&env, &admin, 1, 5).unwrap();
        assert!(!recovered.discarded);
    });
    env.as_contract(&contract_id, || {
        assert_eq!(
            dispatch(&env, effect_spec(&env, 7, 1), 0, None).unwrap(),
            DispatchOutcome::AlreadyPending(1)
        );
        assert!(matches!(
            run_due_effect(&env, &worker, 5, &ScriptedHandler::succeeding()).unwrap(),
            RunOutcome::Succeeded(_)
        ));
        assert_eq!(credit_count(&env, &key), 1);
    });
}

#[test]
fn unknown_effects_are_not_found() {
    let env = Env::default();
    let (contract_id, _admin) = setup(&env);
    let handler = ScriptedHandler::succeeding();
    let worker = Address::generate(&env);

    env.as_contract(&contract_id, || {
        let key = effect_key(&env, 8, 1);
        assert_eq!(effect(&env, &key), None);
        assert_eq!(effect_job(&env, &key), None);
        assert_eq!(
            replay_effect(&env, &worker, &key, 0, &handler),
            Err(JobError::NotFound)
        );
        assert_eq!(effect_dead_letters(&env, None, 10).len(), 0);
    });
}

// ---------------------------------------------------------------------------
// Operational controls
// ---------------------------------------------------------------------------

#[test]
fn a_paused_worker_blocks_new_plans_and_cranks() {
    let env = Env::default();
    let (contract_id, admin) = setup(&env);
    let handler = ScriptedHandler::succeeding();
    let worker = Address::generate(&env);

    let key = effect_key(&env, 9, 1);
    env.as_contract(&contract_id, || {
        dispatch(&env, effect_spec(&env, 9, 1), 0, None).unwrap();

        pause_worker(&env, &admin).unwrap();
        assert_eq!(
            dispatch(&env, effect_spec(&env, 9, 2), 0, None),
            Err(JobError::Disabled)
        );
        assert_eq!(
            run_due_effect(&env, &worker, 0, &handler),
            Err(JobError::Disabled)
        );
    });

    // A separate contract frame: queued work is untouched by the pause.
    env.as_contract(&contract_id, || {
        resume_worker(&env, &admin).unwrap();
        assert!(matches!(
            run_due_effect(&env, &worker, 0, &handler).unwrap(),
            RunOutcome::Succeeded(_)
        ));
        assert_eq!(credit_count(&env, &key), 1);
    });
}

#[test]
fn handlers_with_unguarded_writes_can_double_count() {
    // Documents *why* handlers must read-check-write keyed by the effect key:
    // the dispatcher suppresses replays it can see (after success), but it
    // must retry a failed attempt — including any state that attempt wrote.
    let env = Env::default();
    let (contract_id, _admin) = setup(&env);
    let handler = BlindHandler {
        remaining_failures: Cell::new(2),
    };
    let worker = Address::generate(&env);

    env.as_contract(&contract_id, || {
        let spec = effect_spec(&env, 10, 1);
        dispatch(&env, spec.clone(), 0, Some(policy(3, 10, 40))).unwrap();

        set_ledger(&env, 0);
        assert!(matches!(
            run_due_effect(&env, &worker, 0, &handler).unwrap(),
            RunOutcome::Retried(_)
        ));
        set_ledger(&env, 10);
        assert!(matches!(
            run_due_effect(&env, &worker, 10, &handler).unwrap(),
            RunOutcome::Retried(_)
        ));
        set_ledger(&env, 30);
        assert!(matches!(
            run_due_effect(&env, &worker, 30, &handler).unwrap(),
            RunOutcome::Succeeded(_)
        ));

        let writes: u32 =
            persistent_get(&env, &TestKey::BlindWrites(spec.key.clone())).unwrap_or(0);
        assert_eq!(writes, 3, "three attempts wrote three times");
        assert_eq!(effect_stats(&env).counters.applied_total, 1);

        // The guarded shape from the fixture, for contrast.
        let other = effect_spec(&env, 11, 1);
        dispatch(&env, other.clone(), 0, Some(policy(3, 10, 40))).unwrap();
        let guarded = ScriptedHandler::failing(2, Error::StaleData);
        set_ledger(&env, 0);
        run_due_effect(&env, &worker, 0, &guarded).unwrap();
        set_ledger(&env, 10);
        run_due_effect(&env, &worker, 10, &guarded).unwrap();
        set_ledger(&env, 30);
        run_due_effect(&env, &worker, 30, &guarded).unwrap();
        assert_eq!(
            credit_count(&env, &other.key),
            1,
            "guarded writes stay at one"
        );
    });
}

// ---------------------------------------------------------------------------
// Observability
// ---------------------------------------------------------------------------

/// True when the environment recorded a `("fx", name, key)` event.
fn has_fx_event(env: &Env, name: &Symbol, key: &BytesN<32>) -> bool {
    let events = env.events().all();
    for i in 0..events.len() {
        let (_, topics, _data) = events.get(i).unwrap();
        if topics.len() != 3 {
            continue;
        }
        let Ok(topic0) = Symbol::try_from_val(env, &topics.get(0).unwrap()) else {
            continue;
        };
        let Ok(topic1) = Symbol::try_from_val(env, &topics.get(1).unwrap()) else {
            continue;
        };
        let Ok(topic2) = BytesN::<32>::try_from_val(env, &topics.get(2).unwrap()) else {
            continue;
        };
        if topic0 == FX_TOPIC && topic1 == *name && topic2 == *key {
            return true;
        }
    }
    false
}

#[test]
fn every_transition_is_observable_as_an_fx_event() {
    let env = Env::default();
    let (contract_id, admin) = setup(&env);
    let worker = Address::generate(&env);
    let planned_key = effect_key(&env, 12, 1);
    let recovered_key = effect_key(&env, 12, 2);
    let discarded_key = effect_key(&env, 12, 3);

    env.as_contract(&contract_id, || {
        dispatch(&env, effect_spec(&env, 12, 1), 0, None).unwrap();
        assert!(has_fx_event(&env, &FX_PLANNED, &planned_key));
        assert!(matches!(
            run_due_effect(&env, &worker, 0, &ScriptedHandler::succeeding()).unwrap(),
            RunOutcome::Succeeded(_)
        ));
        assert!(has_fx_event(&env, &FX_APPLIED, &planned_key));
    });

    env.as_contract(&contract_id, || {
        assert!(matches!(
            replay_effect(
                &env,
                &worker,
                &planned_key,
                1,
                &ScriptedHandler::succeeding()
            )
            .unwrap(),
            RunOutcome::AlreadyCompleted(_)
        ));
        assert!(has_fx_event(&env, &FX_REPLAY_SKIPPED, &planned_key));
    });

    env.as_contract(&contract_id, || {
        dispatch(&env, effect_spec(&env, 12, 2), 0, None).unwrap();
        assert!(matches!(
            run_due_effect(
                &env,
                &worker,
                0,
                &ScriptedHandler::failing(u32::MAX, Error::Unauthorized)
            )
            .unwrap(),
            RunOutcome::DeadLettered(_)
        ));
    });

    env.as_contract(&contract_id, || {
        recover_effect(&env, &admin, 2, 5).unwrap();
        assert!(has_fx_event(&env, &FX_REQUEUED, &recovered_key));
    });

    // Another dead letter that is discarded instead of recovered.
    env.as_contract(&contract_id, || {
        dispatch(&env, effect_spec(&env, 12, 3), 0, None).unwrap();
        assert!(matches!(
            run_due_effect(
                &env,
                &worker,
                0,
                &ScriptedHandler::failing(u32::MAX, Error::Unauthorized)
            )
            .unwrap(),
            RunOutcome::DeadLettered(_)
        ));
    });
    env.as_contract(&contract_id, || {
        let job_id = effect(&env, &discarded_key).unwrap().job_id;
        discard_effect(&env, &admin, job_id).unwrap();
        assert!(has_fx_event(&env, &FX_DISCARDED, &discarded_key));
    });
}

// ---------------------------------------------------------------------------
// End-to-end vertical slice
// ---------------------------------------------------------------------------

#[test]
fn settlement_slice_plans_retries_and_applies_a_referral_effect_once() {
    let env = Env::default();
    let (contract_id, _admin) = setup(&env);
    let client = AidSettlementFixtureClient::new(&env, &contract_id);
    let worker = Address::generate(&env);

    set_ledger(&env, 10);
    let created = client.settle_aid(&7, &500);
    assert_eq!(created, DispatchOutcome::Planned(1));
    assert_eq!(client.effect_record(&7).unwrap().payload.len(), 16);

    // Re-running the triggering action does not plan a second effect.
    assert_eq!(
        client.settle_aid(&7, &500),
        DispatchOutcome::AlreadyPending(1)
    );

    // A simulated dependency outage: two retryable failures, then success.
    client.set_failures(&2);
    assert!(matches!(
        client.run_effects(&worker, &10),
        RunOutcome::Retried(_)
    ));
    assert!(matches!(
        client.run_effects(&worker, &70),
        RunOutcome::Retried(_)
    ));
    set_ledger(&env, 190);
    assert!(matches!(
        client.run_effects(&worker, &190),
        RunOutcome::Succeeded(_)
    ));
    assert_eq!(client.sent_referral_count(&7), 1);

    // Both the triggering action and a deliberate replay are idempotent.
    assert_eq!(
        client.settle_aid(&7, &500),
        DispatchOutcome::AlreadyApplied(1)
    );
    assert!(matches!(
        client.replay_aid(&worker, &7, &200),
        RunOutcome::AlreadyCompleted(_)
    ));
    assert_eq!(client.sent_referral_count(&7), 1);
    assert_eq!(client.effect_record(&7).unwrap().applied_ledger, Some(190));

    // A different aid is a different effect with its own lifecycle.
    assert_eq!(client.settle_aid(&8, &250), DispatchOutcome::Planned(2));
    assert_eq!(client.sent_referral_count(&8), 0);
}
