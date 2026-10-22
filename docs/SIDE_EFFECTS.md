# Idempotent side-effect dispatcher (Issue #220)

`shared::dispatcher` is the keyed layer **on top of** the background worker
framework ([`shared::jobs`](WORKERS.md)) for the follow-up work a domain action
triggers: accruing a referral commission after an aid settles, telling the
treasury about a value movement, fanning an attestation out to the oracle,
settling a marketplace order. It exists so that *retrying, replaying, or
recovering* that follow-up can never repeat a user-visible action.

- Module: `shared/src/dispatcher.rs`
- Tests: `shared/src/test_dispatcher.rs` (`cargo test -p shared test_dispatcher`)
- Backing queue: `shared/src/jobs.rs` (retry policy, backoff, dead letters)

## The problem it solves

Inline follow-up work couples the primary action to every consequence: a failed
referral credit makes the aid settlement look failed, and a retried settlement
(the submitter cannot tell whether the first attempt committed) pays the
referral twice. Moving the follow-up to the job queue fixes latency but not
duplication: a replay, a keeper retry, or an operator's `reprocess` call can
still re-run the visible effect.

The dispatcher makes three things true for every side effect:

| Requirement | Mechanism |
| --- | --- |
| Keyed | Deterministic 32-byte key from the effect's identity (`effect_key`, `effect_key_pair`, `effect_key_bytes`) doubles as the job dedupe key |
| Retryable | The backing job carries a `RetryPolicy` captured at plan time; failures retry with exponential backoff |
| Observable | `("fx", <event>, key)` events with the typed `EffectRecord`, plus `effect`, `effect_job`, `effect_stats`, `effect_dead_letters` |
| Recoverable without duplicates | Applied marker checked **before** the handler runs; receipts short-circuit both replay and re-dispatch; dead letters requeue explicitly |

## Lifecycle

```
 dispatch (in the triggering action)      run_due_effect / replay_effect (keeper)
  ─────────────────────────────▶ Planned ──────────────────────▶ Applied
        same invocation             │                                │
        as the domain write         │ handler Err (retryable)        │ EffectRecord.applied_ledger set
                                    ▼                                ▼
                                 Retried ──▶ (attempts == max) ──▶ Dead-lettered
                                    ▲                                │
                                    └──── recover_effect (admin) ◀───┤
                                                                     └─ discard_effect (admin)
```

`EffectRecord` holds the typed view (`kind`, `payload`, `source` correlation,
`planned_ledger`, `applied_ledger`, `discarded`); the backing `Job` holds retry
state (`status`, `attempts`, `next_attempt_ledger`, `last_error`, `worker`).

## Keying rules

Derive the key from the **identity of the work**, never from a counter, a
timestamp, or a request nonce:

```rust
use shared::dispatcher::{effect_key, effect_key_pair, effect_key_bytes, dispatch, EffectSpec, EffectKind};

// One referral accrual per aid, forever.
let key = effect_key(&env, REFERRAL_TAG, aid_id);

// One settlement notice per (epoch, recipient index).
let key = effect_key_pair(&env, SETTLEMENT_TAG, epoch, recipient_index);

// Keys whose identity is not two integers: sha256(tag_be || identity).
let key = effect_key_bytes(&env, NOTICE_TAG, &canonical_identity_bytes);

let spec = EffectSpec {
    kind: EffectKind::ReferralAccrual,
    key,
    payload: amount_bytes,
    source: aid_id, // correlation id back to the triggering action
};
let outcome = dispatch(&env, spec, env.ledger().sequence(), None)?;
```

`dispatch` is a check-then-write over the key:

- first call for a key → `DispatchOutcome::Planned(job_id)`;
- key still queued (or dead-lettered) → `AlreadyPending(job_id)`;
- key already applied → `AlreadyApplied(job_id)`, handler never runs again;
- key discarded by an admin → `Discarded(job_id)`, key stays occupied.

Call `dispatch` **inside the triggering action's invocation** so the domain
state change and the effect plan commit together — either both exist or
neither does.

## Why "applied" is exactly-once

`EffectDispatcher::handle` runs the domain handler and then writes
`EffectRecord.applied_ledger` **in the same contract invocation**. A Soroban
invocation commits entirely or rolls back entirely, so:

- the marker cannot claim an effect applied when its handler rolled back;
- the handler cannot succeed without the marker being written;
- every later path (`dispatch`, `replay_effect`, `recover_effect`, a keeper
  crank) checks the marker or the job receipt *before* invoking the handler.

Consequence for handler authors: the dispatcher suppresses replays it can see,
but it **must** retry a failed attempt. A handler that mutates state and then
returns `Err` will be invoked again. Use read-check-write keyed by
`effect.key`:

```rust
impl EffectHandler for ReferralCreditor {
    fn apply(&self, env: &Env, effect: &EffectRecord) -> Result<(), Error> {
        if credit_exists(env, &effect.key) {
            return Ok(()); // already booked; never double-credit
        }
        credit_referrer(env, &effect.key, /* … */)?;
        Ok(())
    }
}
```

`handlers_with_unguarded_writes_can_double_count` in the test suite
demonstrates the failure this rule prevents.

## Wiring it into a contract

```rust
use shared::dispatcher::{self, EffectHandler, EffectRecord, RunOutcome};
use shared::jobs::{JobError, JobHandler};

// Plan the follow-up in the same call that settles the aid.
pub fn claim_aid(env: Env, aid_id: u64) -> Result<(), AidError> {
    // …settle the aid…
    let key = dispatcher::effect_key(&env, REFERRAL_TAG, aid_id);
    dispatcher::dispatch(
        &env,
        dispatcher::EffectSpec {
            kind: EffectKind::ReferralAccrual,
            key,
            payload: Bytes::new(&env),
            source: aid_id,
        },
        env.ledger().sequence(),
        None,
    )
    .map_err(|_| AidError::RetryScheduled)?;
    Ok(())
}

/// Permissionless crank: one due effect per invocation.
pub fn run_due_effect(env: Env, worker: Address) -> Result<RunOutcome, JobError> {
    dispatcher::run_due_effect(&env, &worker, env.ledger().sequence(), &ReferralCreditor)
}

/// Deliberate replay by key; a replay of an applied effect is a no-op.
pub fn replay_effect(env: Env, worker: Address, key: BytesN<32>) -> Result<RunOutcome, JobError> {
    dispatcher::replay_effect(&env, &worker, &key, env.ledger().sequence(), &ReferralCreditor)
}

pub fn recover(env: Env, admin: Address, job_id: u64) -> Result<EffectRecord, JobError> {
    dispatcher::recover_effect(&env, &admin, job_id, env.ledger().sequence())
}

pub fn discard(env: Env, admin: Address, job_id: u64) -> Result<EffectRecord, JobError> {
    dispatcher::discard_effect(&env, &admin, job_id)
}
```

`run_due_effect` attempts the earliest due job **of any kind**. A contract that
also runs escrow-refund or custom jobs must not hand those payloads to an
effect handler: pass the dispatcher's adapter
(`shared::dispatcher::EffectDispatcher(&handler)`) into a router `JobHandler`
that matches on `job.payload`. The adapter rejects non-effect payloads with
`Error::InvalidArgument`, which is terminal and would dead-letter a foreign
job.

## Events

Topics `("fx", <event>, effect_key)`; the data is an `EffectRecord` (or a
`JobReceipt` for `rplyskip`). Attempt-level transitions also emit the
`("job", …)` events from the worker framework.

| Event | Meaning |
| --- | --- |
| `planned` | Effect recorded and queued |
| `applied` | Handler completed; `applied_ledger` is set (once per key, ever) |
| `requeued` | Admin put a dead letter back on the queue |
| `discarded` | Admin dropped a dead letter; key stays occupied |
| `rplyskip` | A replay found the effect already applied and did nothing |

## Operations runbook

### Diagnosing a dead letter

```bash
# 1. Find the dead-lettered effects (forensic records: attempts, last_error,
#    failed_at_ledger, worker, dedupe key).
stellar contract invoke --id "$CONTRACT_ID" -- effect_dead_letters --cursor null --limit 20

# 2. Read the typed record and the backing job's retry state.
stellar contract invoke --id "$CONTRACT_ID" -- effect --key "$EFFECT_KEY"
stellar contract invoke --id "$CONTRACT_ID" -- effect_job --key "$EFFECT_KEY"

# 3. Health at a glance: pending, dead letters, next due ledger, counters.
stellar contract invoke --id "$CONTRACT_ID" -- effect_stats
```

`last_error` is the contract error code the handler returned. Codes classified
as permanent by `jobs::is_retryable_error` (`Unauthorized`, `NotFound`,
`InvalidArgument`, `InsufficientBalance`, `ContractPaused`, …) dead-letter
immediately rather than burning the retry budget.

### Recovering

1. Fix the cause (fund the treasury, grant the role, repair the target
   contract) **or** decide the effect is no longer wanted.
2. Requeue with a fresh budget:
   `recover_effect(admin, job_id, now)` → emits `requeued`, `attempts = 0`.
3. Re-drive it immediately, or let the next crank pick it up:
   `replay_effect(worker, key, now)` / `run_due_effect(worker)`.
4. Verify: `effect(key).applied_ledger` is `Some(_)`, `effect_stats()
   .counters.applied_total` incremented once, and the target's own record shows
   exactly one user-visible action.

If the effect is genuinely undesired (the triggering action was voided), use
`discard_effect(admin, job_id)`. The job and record stay for audit, the key
stays occupied, and `dispatch` reports `Discarded` so a retry of the triggering
action cannot silently re-plan it. Recovering later clears the discard flag.

### Rolling back

There is no "un-apply" entry point, by design: an applied effect is a fact.
Rollback is a *compensating* action, not a state edit.

1. Never edit storage directly to flip `applied_ledger`; that breaks the
   exactly-once invariant that protects payouts.
2. If an applied effect must be reversed, enqueue a new, separately keyed
   compensating effect (for example a treasury claw-back keyed by
   `(compensation, original_job_id)`) and let it go through the same
   retry/dead-letter path.
3. To stop new work during an incident, call `jobs::pause_worker`: planning and
   cranking both fail closed with `JobError::Disabled`, queued effects stay
   intact, and `jobs::resume_worker` restores service without losing work.
4. If a contract upgrade is the rollback, leave queued effects in place: the
   retry policy travels with each job, and an unchanged handler keeps draining
   the same queue.

## Failure modes and edge cases

| Situation | Behaviour | Operator action |
| --- | --- | --- |
| Handler returns a retryable error | Job rescheduled with exponential backoff; `last_error` set | None; watch `effect_stats().next_due_ledger` |
| Handler returns a permanent error | Dead-lettered on that attempt (no retries burned) | Fix the cause, then recover |
| Attempt budget exhausted | Dead-lettered with `attempts`, `worker`, ledger, error code | Diagnose, then recover or discard |
| Replay of an applied effect | `RunOutcome::AlreadyCompleted(receipt)`; handler not invoked; `replay_skips_total` increments | None |
| Replay of a dead letter | `JobError::NotRetryable` | `recover_effect` first (keeps recovery audited) |
| Re-dispatch after apply | `AlreadyApplied(job_id)`; no new job | None |
| Re-dispatch after discard | `Discarded(job_id)`; key stays occupied | `recover_effect` if it is wanted again |
| Worker paused | `dispatch` and cranks fail with `JobError::Disabled` | `jobs::resume_worker` |
| Payload over 1024 bytes | `JobError::InvalidPayload` | Keep payloads as ids/amounts; read state in the handler |
| Queue holds non-effect jobs | `run_due_effect` can pick them; the effect adapter rejects them and the job dead-letters | Use a router `JobHandler` in mixed contracts |
| Key never dispatched | `effect`/`effect_job` → `None`; replay → `JobError::NotFound` | Check the key derivation matches the triggering action |
| Storage eviction | Records, jobs, receipts and dead letters are persistent entries bumped on every access | Crank regularly; receipts also exist for completed effects |

### Cost notes

- `dispatch` is one check of the receipt plus one check of the key owner, then
  the job enqueue: O(1) in queue depth.
- Cranking attempts one effect per invocation and scans the pending index
  (`O(pending)` reads), same trade-off as `shared::jobs`. Run several keepers
  for liveness; the keys and receipts, not the operator, provide safety.
- `effect_stats` scans the pending and dead-letter indexes; call it from
  dashboards and incident tooling, not from a hot path.

## Compatibility

Additive: new entry points, new storage keys under the `("fx", …)` namespace,
new `JobPayload::Effect` / `JobKind::Effect` variants and one new
`JobError::InvalidPayload` code. No existing contract, event, or storage layout
changes, so no migration is required. Enums in this crate are `#[contracttype]`
values — an old reader that never receives an effect payload is unaffected.

## Local development

```bash
cargo test -p shared test_dispatcher   # dispatcher suite (13 tests)
cargo test -p shared test_jobs         # worker framework suite
```

The end-to-end test `settlement_slice_plans_retries_and_applies_a_referral_effect_once`
exercises a fixture contract the way a real module would: settle an aid, plan
the keyed referral effect, fail twice on a simulated outage, apply once,
re-dispatch and replay without duplicating the referral credit.
