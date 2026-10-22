//! Idempotent side-effect dispatcher with replay and dead-letter recovery
//! (Issue #220).
//!
//! Domain actions such as settling an aid disbursement or releasing an escrow
//! usually need to trigger *further* user-visible work: accrue a referral
//! commission, notify the treasury, attest to the oracle, settle a
//! marketplace order. Doing that work inline couples the primary action to
//! every one of its follow-ups, and retrying the primary action — or a keeper
//! replaying a request — can repeat a payout that already happened.
//!
//! `shared::dispatcher` is the keyed layer *on top of* [`crate::jobs`] that
//! makes follow-up work safe to retry, replay, and recover:
//!
//! - **Keyed.** Every side effect carries a deterministic 32-byte key derived
//!   from its logical identity (see [`effect_key`]). Planning, applying,
//!   replaying and re-dispatching all collide on that one key, so the same
//!   logical effect exists at most once and applies at most once.
//! - **Retryable.** Backing jobs inherit the whole [`crate::jobs`] retry
//!   machinery: per-job [`RetryPolicy`] captured at plan time, exponential
//!   backoff, and a dead-letter queue with forensic context.
//! - **Observable.** Every transition publishes `("fx", <event>, key)` with
//!   the typed [`EffectRecord`] as data, and [`effect_stats`] /
//!   [`effect_dead_letters`] expose queue health without storage reads by
//!   indexers.
//! - **Recoverable without duplicate actions.** [`recover_effect`] requeues a
//!   dead letter with a fresh attempt budget; [`replay_effect`] re-drives any
//!   effect on demand. Both are safe on already-applied effects: the applied
//!   marker is checked *before* the domain handler runs, so a replay returns
//!   the receipt instead of repeating the user-visible action.
//!
//! ## Why the "applied" marker is trustworthy
//!
//! The domain handler ([`EffectHandler::apply`]) and the [`EffectRecord`]
//! applied-marker write happen in the **same contract invocation**. On
//! Soroban, an invocation either commits entirely or rolls back entirely, so
//! the marker can never say "applied" for an effect whose handler rolled back,
//! and a handler can never succeed without the marker being written. That is
//! the exactly-once argument: one ledger transaction = one application.
//!
//! Handlers must still be *effectively* idempotent for their own storage:
//! a handler that writes state and then returns `Err` gets retried (in unit
//! tests the write survives; on-chain the whole invocation rolls back, but a
//! handler that catches a failed sub-call must still read-check-write). Guard
//! per-effect state with its own key, as the tests in `test_dispatcher.rs`
//! demonstrate.
//!
//! ## Relationship to `shared::jobs`
//!
//! `jobs` answers *when and how often* work runs; `dispatcher` answers *whether
//! it may run again at all*. Contracts that only need delayed work should use
//! `jobs` directly. Contracts that fan out user-visible follow-ups should plan
//! them through this module, which plans them as `JobPayload::Effect` backing
//! jobs.
//!
//! See `docs/SIDE_EFFECTS.md` for the operator runbook: diagnosing dead
//! letters, replaying, recovering, and rolling back.

use soroban_sdk::{contracttype, symbol_short, Address, Bytes, BytesN, Env, Symbol, Vec};

use crate::errors::Error;
use crate::jobs::{
    self, DeadLetterRecord, EnqueueOutcome, Job, JobError, JobHandler, JobKind, JobPayload,
    JobReceipt, RetryPolicy, RunOutcome,
};
use crate::storage::{instance_get, instance_set, persistent_get, persistent_set};

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

/// Largest accepted effect payload, in bytes.
///
/// An effect payload is an argument list for the domain handler, not a data
/// store; keep it small (ids, amounts, addresses) and read the rest from
/// canonical storage at apply time.
pub const MAX_EFFECT_PAYLOAD_BYTES: u32 = 1024;

// ---------------------------------------------------------------------------
// Effect classification and specification
// ---------------------------------------------------------------------------

/// Low-cardinality effect classification, used for events, dashboards, and
/// routing in contracts that mix module concerns.
#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EffectKind {
    /// Aid-module follow-up (settlement notification, bookkeeping).
    AidSettlement,
    /// Referral-module commission accrual.
    ReferralAccrual,
    /// Treasury-module bookkeeping after a value movement.
    TreasuryNotice,
    /// Oracle-module attestation fan-out.
    OracleAttestation,
    /// Marketplace-module settlement follow-up.
    MarketplaceSettlement,
    /// Governance-module notice.
    GovernanceNotice,
    /// Caller-defined classification for work that does not fit above.
    Custom(u32),
}

impl EffectKind {
    /// Stable symbol used in effect events.
    pub fn as_symbol(&self) -> Symbol {
        match self {
            EffectKind::AidSettlement => symbol_short!("aid_set"),
            EffectKind::ReferralAccrual => symbol_short!("ref_acc"),
            EffectKind::TreasuryNotice => symbol_short!("treasury"),
            EffectKind::OracleAttestation => symbol_short!("oracle"),
            EffectKind::MarketplaceSettlement => symbol_short!("mkt_set"),
            EffectKind::GovernanceNotice => symbol_short!("govern"),
            EffectKind::Custom(_) => symbol_short!("custom"),
        }
    }
}

/// A side effect as requested by the domain code that triggered it.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectSpec {
    /// Classification for routing and dashboards.
    pub kind: EffectKind,
    /// Idempotency key: derive it deterministically from the identity of the
    /// work with [`effect_key`], [`effect_key_pair`] or [`effect_key_bytes`],
    /// never from a counter, a timestamp, or a request nonce.
    pub key: BytesN<32>,
    /// Opaque arguments for the domain handler. At most
    /// [`MAX_EFFECT_PAYLOAD_BYTES`].
    pub payload: Bytes,
    /// Correlation id back to the action that produced this effect (an aid
    /// id, an escrow id, the ledger of the triggering call, …).
    pub source: u64,
}

/// The dispatcher's typed view of one planned (or applied) effect.
///
/// Stored under the `("fx", effect_key)` storage key; the backing job holds
/// the retry state (`attempts`, `next_attempt_ledger`, `last_error`, status).
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectRecord {
    /// Id of the backing `jobs` job.
    pub job_id: u64,
    /// Classification captured at plan time.
    pub kind: EffectKind,
    /// Idempotency key of this effect.
    pub key: BytesN<32>,
    /// Handler arguments captured at plan time.
    pub payload: Bytes,
    /// Correlation id of the triggering action.
    pub source: u64,
    /// Ledger the effect was planned on.
    pub planned_ledger: u32,
    /// Ledger the effect was applied on. `None` while the effect is pending
    /// or dead-lettered; once set, the effect must never run again.
    pub applied_ledger: Option<u32>,
    /// Set when an admin discarded its dead letter: the key stays occupied so
    /// the same logical effect can never be planned again, and further
    /// [`dispatch`] calls report [`DispatchOutcome::Discarded`]. Cleared by
    /// [`recover_effect`].
    pub discarded: bool,
}

/// Result of planning a side effect with [`dispatch`].
#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchOutcome {
    /// A new effect job was planned; carries the backing job id.
    Planned(u64),
    /// An equivalent effect is already queued; carries the existing job id.
    AlreadyPending(u64),
    /// The effect already applied; carries the backing job id. The handler
    /// will not run again for this key.
    AlreadyApplied(u64),
    /// The effect was dead-lettered and its dead letter was discarded by an
    /// admin; carries the backing job id. The key stays occupied, so a new
    /// effect is never planned for it.
    Discarded(u64),
}

// ---------------------------------------------------------------------------
// Counters and stats
// ---------------------------------------------------------------------------

/// Running totals for dispatcher activity (instance storage).
#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EffectCounters {
    /// Effects planned for the first time.
    pub planned_total: u64,
    /// Effects whose handler completed (once per key, ever).
    pub applied_total: u64,
    /// [`replay_effect`] calls that found the effect already applied and
    /// skipped the handler.
    pub replay_skips_total: u64,
}

impl EffectCounters {
    pub const fn zero() -> EffectCounters {
        EffectCounters {
            planned_total: 0,
            applied_total: 0,
            replay_skips_total: 0,
        }
    }
}

/// Inspection view of the effect queue: dispatcher counters plus the backing
/// job queue depth (effects only, even in contracts that also run other job
/// kinds).
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectStats {
    pub counters: EffectCounters,
    /// Effects waiting for an attempt.
    pub pending: u32,
    /// Effects that exhausted their retries.
    pub dead_letters: u32,
    /// Earliest `next_attempt_ledger` across pending effects, if any.
    pub next_due_ledger: Option<u32>,
}

// ---------------------------------------------------------------------------
// Storage keys and events
// ---------------------------------------------------------------------------

// Storage keys.
//
// `#[contracttype]` enum keys encode as their variant index only, so an enum
// key would collide with any other enum key that happens to share the index
// (for example `jobs::WorkerKey::Counter`). The dispatcher therefore
// namespaces its keys under the `("fx", …)` tuple instead:
//
// - `("fx", effect_key)` → `EffectRecord` (persistent)
// - `("fx", "counters")` → `EffectCounters` (instance)

fn record_storage_key(key: &BytesN<32>) -> (Symbol, BytesN<32>) {
    (FX_TOPIC, key.clone())
}

fn counters_storage_key() -> (Symbol, Symbol) {
    (FX_TOPIC, symbol_short!("counters"))
}

/// Topic prefix for every dispatcher event.
pub const FX_TOPIC: Symbol = symbol_short!("fx");
/// A side effect was planned.
pub const FX_PLANNED: Symbol = symbol_short!("planned");
/// A side effect's handler completed; the applied marker is written.
pub const FX_APPLIED: Symbol = symbol_short!("applied");
/// A dead-lettered effect was requeued by an admin.
pub const FX_REQUEUED: Symbol = symbol_short!("requeued");
/// A dead-lettered effect was discarded by an admin.
pub const FX_DISCARDED: Symbol = symbol_short!("discarded");
/// A replay found the effect already applied and skipped the handler.
pub const FX_REPLAY_SKIPPED: Symbol = symbol_short!("rplyskip");

/// Topics: `("fx", event, effect_key)`; data: `EffectRecord`.
fn publish_record_event(env: &Env, event: Symbol, key: &BytesN<32>, record: &EffectRecord) {
    env.events()
        .publish((FX_TOPIC, event, key.clone()), record.clone());
}

/// Topics: `("fx", event, effect_key)`; data: `JobReceipt`.
fn publish_receipt_event(env: &Env, event: Symbol, key: &BytesN<32>, receipt: &JobReceipt) {
    env.events()
        .publish((FX_TOPIC, event, key.clone()), receipt.clone());
}

// ---------------------------------------------------------------------------
// Effect keys
// ---------------------------------------------------------------------------

/// Deterministic effect key from a tag and a single domain id.
///
/// ```ignore
/// // one referral-accrual effect per aid, forever
/// let key = effect_key(&env, AID_SETTLEMENT_TAG, aid_id);
/// ```
///
/// The key is built structurally (tag in bytes 0..4, value in bytes 4..12), so
/// two different `(tag, value)` pairs can never collide.
pub fn effect_key(env: &Env, tag: u32, id: u64) -> BytesN<32> {
    jobs::dedupe_key_u64(env, tag, id)
}

/// Deterministic effect key from a tag and two domain ids, e.g. one effect per
/// `(settlement_epoch, recipient_index)`.
pub fn effect_key_pair(env: &Env, tag: u32, first: u64, second: u64) -> BytesN<32> {
    jobs::dedupe_key_pair(env, tag, first, second)
}

/// Deterministic effect key from a tag and arbitrary identifying bytes:
/// `sha256(tag_be || identity)`.
///
/// Use this when the effect's identity is not two integers (a contract
/// address, a claim hash, a canonicalized argument tuple).
pub fn effect_key_bytes(env: &Env, tag: u32, identity: &Bytes) -> BytesN<32> {
    let mut message = Bytes::from_slice(env, &tag.to_be_bytes());
    message.append(identity);
    env.crypto().sha256(&message).to_bytes()
}

// ---------------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------------

/// Plan a side effect unless an equivalent one is already queued or applied.
///
/// Call this from inside the triggering action's invocation so the primary
/// state change and the effect plan commit atomically. `due_ledger` is the
/// earliest ledger the effect may run on; use the current ledger for
/// immediate work. `policy` is captured in the backing job, so later
/// configuration changes never alter already-planned work.
///
/// Idempotency: planning the same key again returns [`DispatchOutcome::
/// AlreadyPending`] while it is queued and [`DispatchOutcome::AlreadyApplied`]
/// once it has run — a duplicate plan never creates a second job.
pub fn dispatch(
    env: &Env,
    spec: EffectSpec,
    due_ledger: u32,
    policy: Option<RetryPolicy>,
) -> Result<DispatchOutcome, JobError> {
    if spec.payload.len() > MAX_EFFECT_PAYLOAD_BYTES {
        return Err(JobError::InvalidPayload);
    }

    // An applied effect is never re-planned; the receipt is the source of
    // truth that the user-visible action already happened.
    if let Some(receipt) = jobs::get_receipt(env, &spec.key) {
        synthesize_record_if_missing(env, &spec, &receipt);
        return Ok(DispatchOutcome::AlreadyApplied(receipt.job_id));
    }

    // A discarded effect stays discarded: the key is deliberately kept
    // occupied so the unwanted work cannot be re-planned by a retry of the
    // triggering action. Recover it first if it is wanted again.
    if let Some(existing) = load_effect(env, &spec.key) {
        if existing.discarded && existing.applied_ledger.is_none() {
            return Ok(DispatchOutcome::Discarded(existing.job_id));
        }
    }

    match jobs::enqueue_job(
        env,
        JobPayload::Effect(spec.key.clone()),
        spec.key.clone(),
        due_ledger,
        policy,
    )? {
        EnqueueOutcome::Created(job_id) => {
            let record = EffectRecord {
                job_id,
                kind: spec.kind,
                key: spec.key.clone(),
                payload: spec.payload,
                source: spec.source,
                planned_ledger: env.ledger().sequence(),
                applied_ledger: None,
                discarded: false,
            };
            save_effect(env, &record);
            bump_counters(env, |c| {
                c.planned_total = c.planned_total.saturating_add(1);
            });
            publish_record_event(env, FX_PLANNED, &record.key, &record);
            Ok(DispatchOutcome::Planned(job_id))
        }
        EnqueueOutcome::AlreadyPending(job_id) => Ok(DispatchOutcome::AlreadyPending(job_id)),
        // The receipt appeared between the check above and the enqueue (same
        // ledger, concurrent crank). Treat exactly like AlreadyApplied.
        EnqueueOutcome::AlreadyCompleted(job_id) => Ok(DispatchOutcome::AlreadyApplied(job_id)),
    }
}

// ---------------------------------------------------------------------------
// Domain handlers and the crank
// ---------------------------------------------------------------------------

/// Domain side effect executed by the dispatcher.
///
/// Implementations receive the full [`EffectRecord`] (kind, payload, source
/// correlation) and must:
///
/// 1. **Read-check-write.** Keep any per-effect bookkeeping keyed by
///    `effect.key` and check it before mutating, so a retried handler after a
///    partial failure can never double-count.
/// 2. **Return errors instead of trapping**, so failure context (the contract
///    error code) lands on the job and classifies as retryable or terminal
///    through `jobs::is_retryable_error`.
pub trait EffectHandler {
    fn apply(&self, env: &Env, effect: &EffectRecord) -> Result<(), Error>;
}

/// Adapter that runs an [`EffectHandler`] inside the `jobs` crank.
///
/// Contracts with mixed job kinds should embed this in a router `JobHandler`
/// that matches on `job.payload`; contracts that only run effects can pass
/// their handler straight to [`run_due_effect`].
pub struct EffectDispatcher<'a, H: EffectHandler>(pub &'a H);

impl<H: EffectHandler> JobHandler for EffectDispatcher<'_, H> {
    fn handle(&self, env: &Env, job: &Job) -> Result<(), Error> {
        let JobPayload::Effect(key) = &job.payload else {
            return Err(Error::InvalidArgument);
        };
        let Some(record) = load_effect(env, key) else {
            return Err(Error::NotFound);
        };
        // The applied marker is the no-duplicate guarantee: once set, the
        // domain handler is never invoked again for this key.
        if record.applied_ledger.is_some() {
            return Ok(());
        }
        self.0.apply(env, &record)?;
        let mut applied = record;
        applied.applied_ledger = Some(env.ledger().sequence());
        save_effect(env, &applied);
        bump_counters(env, |c| {
            c.applied_total = c.applied_total.saturating_add(1);
        });
        publish_record_event(env, FX_APPLIED, &applied.key, &applied);
        Ok(())
    }
}

/// Attempt the earliest due side effect, if any.
///
/// Permissionless, like the rest of the crank: the calling address is recorded
/// on the backing job for accountability. Safe for anyone to call — replays
/// and already-applied effects are absorbed by the dedupe keys, receipts, and
/// the applied marker.
///
/// Note: this picks the earliest due job of *any* kind. Contracts that also
/// enqueue escrow-refund or custom jobs should use their own router
/// `JobHandler` with `jobs::run_due_job` instead, so a foreign payload is
/// never handed to an effect handler.
pub fn run_due_effect<H: EffectHandler>(
    env: &Env,
    worker: &Address,
    now: u32,
    handler: &H,
) -> Result<RunOutcome, JobError> {
    let dispatcher = EffectDispatcher(handler);
    jobs::run_due_job(env, worker, now, &dispatcher)
}

/// Re-drive one effect by key, ignoring its schedule.
///
/// This is the deliberate replay entry point: after fixing a handler, after
/// disputing a dead letter, or during incident recovery.
///
/// - Effect already applied → returns [`RunOutcome::AlreadyCompleted`] with
///   the receipt; the handler is **not** invoked, so a replay can never
///   repeat the user-visible action. The skip is counted and emitted.
/// - Effect queued → attempted now (same retry/backoff rules as the crank).
/// - Effect dead-lettered → [`JobError::NotRetryable`]; requeue it with
///   [`recover_effect`] first, so recovery stays an explicit, audited admin
///   action.
/// - Unknown key → [`JobError::NotFound`].
pub fn replay_effect<H: EffectHandler>(
    env: &Env,
    worker: &Address,
    key: &BytesN<32>,
    now: u32,
    handler: &H,
) -> Result<RunOutcome, JobError> {
    let record = load_effect(env, key).ok_or(JobError::NotFound)?;
    if let Some(receipt) = jobs::get_receipt(env, key) {
        bump_counters(env, |c| {
            c.replay_skips_total = c.replay_skips_total.saturating_add(1);
        });
        publish_receipt_event(env, FX_REPLAY_SKIPPED, key, &receipt);
        return Ok(RunOutcome::AlreadyCompleted(receipt));
    }
    let dispatcher = EffectDispatcher(handler);
    jobs::reprocess_job(env, worker, record.job_id, now, &dispatcher)
}

// ---------------------------------------------------------------------------
// Dead-letter recovery
// ---------------------------------------------------------------------------

/// Put a dead-lettered effect back on the queue with a fresh attempt budget.
/// Admin only.
///
/// Recovery is always safe with respect to duplicate actions: the backing job
/// keeps its dedupe key and the effect keeps its applied marker, so if the
/// original attempts had actually succeeded the replay short-circuits on the
/// receipt instead of re-running the handler.
pub fn recover_effect(
    env: &Env,
    caller: &Address,
    job_id: u64,
    now: u32,
) -> Result<EffectRecord, JobError> {
    // Resolve the effect before touching the queue, so a missing record or a
    // non-effect payload never leaves a half-applied recovery behind.
    let job = jobs::get_job(env, job_id).ok_or(JobError::NotFound)?;
    let key = effect_key_of(&job)?;
    let mut record = load_effect(env, &key).ok_or(JobError::NotFound)?;
    jobs::requeue_dead_letter(env, caller, job_id, now)?;
    if record.discarded {
        record.discarded = false;
        save_effect(env, &record);
    }
    publish_record_event(env, FX_REQUEUED, &key, &record);
    Ok(record)
}

/// Drop a dead-lettered effect from the queue, keeping its job and record for
/// audit. Admin only.
///
/// Use this when the effect is genuinely undesired (for example the triggering
/// action was voided); the dedupe key stays occupied, so the same logical
/// effect can never be planned again by mistake.
pub fn discard_effect(env: &Env, caller: &Address, job_id: u64) -> Result<EffectRecord, JobError> {
    let job = jobs::get_job(env, job_id).ok_or(JobError::NotFound)?;
    let key = effect_key_of(&job)?;
    let mut record = load_effect(env, &key).ok_or(JobError::NotFound)?;
    jobs::discard_dead_letter(env, caller, job_id)?;
    record.discarded = true;
    save_effect(env, &record);
    publish_record_event(env, FX_DISCARDED, &key, &record);
    Ok(record)
}

// ---------------------------------------------------------------------------
// Inspection
// ---------------------------------------------------------------------------

/// Read one effect record by key.
pub fn effect(env: &Env, key: &BytesN<32>) -> Option<EffectRecord> {
    load_effect(env, key)
}

/// Read the backing job for an effect (retry state: attempts, schedule,
/// status, last error).
pub fn effect_job(env: &Env, key: &BytesN<32>) -> Option<Job> {
    load_effect(env, key).and_then(|record| jobs::get_job(env, record.job_id))
}

/// Forensic records for dead-lettered effects, newest ids first.
///
/// `cursor` and `limit` page over the backing dead-letter queue; in contracts
/// that dead-letter other job kinds as well, non-effect entries are filtered
/// out, so a page may hold fewer than `limit` entries.
pub fn effect_dead_letters(env: &Env, cursor: Option<u64>, limit: u32) -> Vec<DeadLetterRecord> {
    let candidates = jobs::list_dead_letters(env, cursor, limit);
    let mut effects = Vec::new(env);
    for record in candidates.iter() {
        if record.kind == JobKind::Effect {
            effects.push_back(record);
        }
    }
    effects
}

/// Dispatcher counters plus backing queue depth (effects only).
pub fn effect_stats(env: &Env) -> EffectStats {
    let mut pending = 0u32;
    let mut next_due_ledger: Option<u32> = None;
    for id in jobs::pending_job_ids(env).iter() {
        let Some(job) = jobs::get_job(env, id) else {
            continue;
        };
        if job.kind != JobKind::Effect {
            continue;
        }
        pending = pending.saturating_add(1);
        next_due_ledger = Some(match next_due_ledger {
            Some(current) if current <= job.next_attempt_ledger => current,
            _ => job.next_attempt_ledger,
        });
    }
    let mut dead_letters = 0u32;
    for id in jobs::dead_letter_job_ids(env).iter() {
        if let Some(job) = jobs::get_job(env, id) {
            if job.kind == JobKind::Effect {
                dead_letters = dead_letters.saturating_add(1);
            }
        }
    }
    EffectStats {
        counters: load_counters(env),
        pending,
        dead_letters,
        next_due_ledger,
    }
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

fn effect_key_of(job: &Job) -> Result<BytesN<32>, JobError> {
    match &job.payload {
        JobPayload::Effect(key) => Ok(key.clone()),
        _ => Err(JobError::InvalidPayload),
    }
}

fn synthesize_record_if_missing(env: &Env, spec: &EffectSpec, receipt: &JobReceipt) {
    if load_effect(env, &spec.key).is_some() {
        return;
    }
    // The receipt proves the effect applied (marker and receipt are written in
    // the same invocation); reconstruct the typed record for observability.
    let record = EffectRecord {
        job_id: receipt.job_id,
        kind: spec.kind,
        key: spec.key.clone(),
        payload: spec.payload.clone(),
        source: spec.source,
        planned_ledger: receipt.completed_ledger,
        applied_ledger: Some(receipt.completed_ledger),
        discarded: false,
    };
    save_effect(env, &record);
}

fn load_effect(env: &Env, key: &BytesN<32>) -> Option<EffectRecord> {
    persistent_get(env, &record_storage_key(key))
}

fn save_effect(env: &Env, record: &EffectRecord) {
    persistent_set(env, &record_storage_key(&record.key), record);
}

fn load_counters(env: &Env) -> EffectCounters {
    instance_get(env, &counters_storage_key()).unwrap_or_else(EffectCounters::zero)
}

fn bump_counters<F: FnOnce(&mut EffectCounters)>(env: &Env, update: F) {
    let mut counters = load_counters(env);
    update(&mut counters);
    instance_set(env, &counters_storage_key(), &counters);
}
