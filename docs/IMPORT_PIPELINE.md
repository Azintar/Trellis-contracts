# Bulk Import Pipeline & Rollback Guidance (Issue #37)

The Bulk Import Pipeline provides a secure, deterministic, and idempotent mechanism for importing large datasets (e.g. beneficiary lists, donor allocations, registry mappings) into Trellis Soroban contracts. It features pre-flight **dry-run validation**, **duplicate detection**, **atomic or best-effort execution modes**, and prescriptive **rollback guidance**.

The core implementation lives in [`shared::import`](../shared/src/import.rs) with contract-level endpoints exposed on [`contracts/aid-contract`](../contracts/aid-contract/src/lib.rs).

---

## 1. Overview & Key Capabilities

| Capability | Implementation | Benefit |
|------------|----------------|---------|
| **Dry-Run Validation** | `dry_run(&env, &items, &config)` | Zero persistent writes; returns create, update, skip, and error counts prior to committing state. |
| **Idempotency** | External ID tracking in persistent storage | Repeated submissions of the same batch produce zero duplicate records. |
| **Duplicate Policies** | `SkipExisting`, `UpdateExisting`, `RejectDuplicate` | Configurable duplicate resolution matching the operator's business requirements. |
| **Execution Modes** | `AllOrNothing` (atomic) vs `BestEffort` (partial) | Support for either strict all-or-none integrity or resilient partial rollout with row errors. |
| **Rollback Guidance** | Automated `RollbackGuidance` in report | Evaluates import state and gives operators prescriptive runbook codes and remediation actions. |
| **Gas & Size Safety** | Enforced `ABSOLUTE_MAX_IMPORT_SIZE = 100` | Protects Soroban CPU and memory budgets from oversized batch failures. |
| **Reentrancy Protection** | Temporary storage lock `imp_lck` | Prevents reentrant invocations during batch execution. |

---

## 2. Supported Import Format & Schema

Each import payload consists of a list of `ImportItem` rows conforming to the canonical schema:

```rust
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportItem {
    /// Zero-based row sequence number within the input batch.
    pub row_id: u32,
    /// External client/NGO business identifier for idempotency & deduplication.
    pub external_id: Option<Bytes>,
    /// Target recipient address.
    pub recipient: Address,
    /// Disbursement or allocation amount (must be > 0).
    pub amount: i128,
    /// Expiration ledger sequence or timestamp (must be > current ledger).
    pub expiry: u64,
    /// Optional payload / metadata hash (e.g. SHA-256 of off-chain metadata).
    pub metadata_hash: Option<Bytes>,
}
```

### JSON / Off-Chain Format Example
External systems submitting data via SDK or CLI can structure records as follows:
```json
[
  {
    "row_id": 0,
    "external_id": "BENEFICIARY-2026-001",
    "recipient": "GDG44Y6F7NKG...W54A",
    "amount": "500000000",
    "expiry": 1790600000,
    "metadata_hash": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
  },
  {
    "row_id": 1,
    "external_id": "BENEFICIARY-2026-002",
    "recipient": "GBQ2VNYEWM6Q...Z29K",
    "amount": "750000000",
    "expiry": 1790600000,
    "metadata_hash": null
  }
]
```

### Validation Rules

Before any row is processed or written, the pipeline evaluates the following constraints:

1. **Batch Size Limits**:
   - `items.len() > 0`: An empty batch is rejected with `ImportError::EmptyBatch` (`941`).
   - `items.len() <= config.max_rows`: Batches exceeding the configured ceiling or `ABSOLUTE_MAX_IMPORT_SIZE` (100) are rejected with `ImportError::BatchTooLarge` (`940`).
2. **Amount Positive**:
   - `item.amount > 0`: Zero or negative amounts are rejected with `ImportError::InvalidAmount` (`946`, reason `zero_amt`).
3. **Future Expiration**:
   - `item.expiry > current_ledger_timestamp`: Records whose expiration is in the past are rejected with `ImportError::ExpiredRecord` (`947`, reason `expired`).
4. **External ID Bounded Length**:
   - When provided, `external_id.len() > 0` and `external_id.len() <= 64 bytes`: Oversized IDs are rejected with `ImportError::ExternalIdTooLong` (`948`, reason `bad_id`).
5. **Recipient Address**:
   - Must be a structurally valid Soroban `Address`.

---

## 3. Duplicate Detection & Idempotency

External IDs provide deterministic replay protection:

### Resolution Policies (`DuplicatePolicy`)

- **`SkipExisting`** (Default for idempotent replays):
  - If a record with `external_id` already exists in storage or earlier in the batch:
  - Leaves the existing record untouched.
  - Increments `report.skip_count`.
  - Does not count as an error.
- **`UpdateExisting`**:
  - If a record with `external_id` already exists in storage or earlier in the batch:
  - Updates the stored recipient, amount, expiry, and metadata hash.
  - Increments `report.update_count`.
- **`RejectDuplicate`**:
  - If an `external_id` appears more than once in the batch or already exists in persistent storage:
  - Increments `report.error_count`.
  - Records a `RowError` with reason `dup_batch` (if duplicate in input batch) or `dup_store` (if duplicate against existing storage).

---

## 4. Dry-Run Execution (Preview Without Writes)

### Guarantee: Zero Persistent Writes
When `config.dry_run == true`:
- The pipeline executes full validation against all rows.
- Duplicate detection is performed using **read-only queries** (`persistent_read`) and **in-memory tracking** (`Map<Bytes, u32>`).
- No records are inserted, modified, or deleted in contract storage.
- Storage counters remain unchanged.
- Emits an `("import", "sim")` ledger event for indexer telemetry.

### Return Type (`ImportReport`)

```rust
pub struct ImportReport {
    pub is_dry_run: bool,
    pub total_rows: u32,
    pub create_count: u32,
    pub update_count: u32,
    pub skip_count: u32,
    pub error_count: u32,
    pub errors: Vec<RowError>,
    pub rollback_guidance: RollbackGuidance,
    pub batch_fingerprint: BytesN<32>,
}
```

---

## 5. Rollback Guidance & Remediation Runbook

The `ImportReport` includes a `RollbackGuidance` object that prescribes the exact recovery action:

```rust
pub struct RollbackGuidance {
    pub strategy: RollbackStrategy,
    pub action: Symbol,
    pub affected_records: u32,
    pub runbook_code: Symbol,
}
```

### Remediation Matrix

| Scenario | Mode | `strategy` | `action` | Runbook Code | Operator Remediation |
|----------|------|------------|----------|--------------|----------------------|
| **Clean Import** | Any | `Manual` | `no_action` | `clean` | All rows succeeded. No remediation required. |
| **Dry-Run Detected Errors** | Dry Run | `ForwardFix` | `fix_rows` | `dry_err` | Fix invalid rows in source file before running commit. |
| **AllOrNothing Failure** | `AllOrNothing` | `Revert` | `reverted` | `atom_rev` | Entire transaction was aborted. Zero state persisted. Fix invalid records and resubmit the batch. |
| **Partial Failure** | `BestEffort` | `ForwardFix` | `part_fix` | `part_rem` | Valid rows committed; invalid rows skipped. Follow the partial remediation runbook below. |

---

### Step-by-Step Remediation Runbooks

#### Runbook A: Remediation for Invalid Rows (`dry_err` / `atom_rev`)
1. Inspect `report.errors` to locate failing rows by `row_id` and `external_id`.
2. Inspect `reason`:
   - `zero_amt`: Update amount to a strictly positive value (`> 0`).
   - `expired`: Update expiration timestamp to a future ledger time.
   - `bad_id`: Ensure external identifier is between 1 and 64 bytes.
   - `dup_batch`: Remove intra-batch duplicate row or change duplicate policy to `SkipExisting` / `UpdateExisting`.
   - `dup_store`: Record already exists on-chain. Verify if update was intended (`UpdateExisting`) or already processed (`SkipExisting`).
3. Correct the source dataset and re-execute dry-run.

#### Runbook B: Remediation for Partial Imports (`part_rem`)
When running in `BestEffort` mode, valid rows are durably written while invalid rows are returned in `report.errors`:
1. **Extract Failed Rows**:
   Extract all `row_id` entries listed in `report.errors`. The valid rows (`create_count` + `update_count`) have already been safely committed.
2. **Correct Data Fields**:
   Fix the invalid fields (e.g. adjust expired dates, fix amounts) in a delta batch.
3. **Resubmit Delta Batch**:
   Submit only the corrected failed rows in a new import request.
4. **Safe Re-submission of Whole Batch**:
   Alternatively, operators can safely resubmit the entire original file with `DuplicatePolicy::SkipExisting`. The previously committed records will be recognized by their external IDs and safely skipped (`skip_count`), while the newly corrected rows will be committed (`create_count`).

---

## 6. Contract API Reference

### Aid Contract Integration (`contracts/aid-contract`)

```rust
// 1. Dry-run preview
let report = client.import_aids_dry_run(&items, &config);

// 2. Commit execution
let report = client.import_aids(&caller, &items, &config);

// 3. Query imported record by external ID
let record = client.get_imported_aid(&external_id);
```

### Shared Module (`shared::import`)

```rust
use shared::import::{
    dry_run, execute_import, get_imported_record, has_imported_record,
    DuplicatePolicy, ImportConfig, ImportItem, ImportMode, ImportReport,
};
```

---

## 7. Automated Testing & Validation

Run the automated test suite across the workspace:

```bash
# Test shared import module (dry-run, idempotency, validation, duplicates, limits)
cargo test -p shared --lib test_import

# Test aid-contract end-to-end contract calls and authorization
cargo test -p aid-contract
```
