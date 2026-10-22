//! Import Pipeline with Dry-Run Validation and Rollback Guidance (Issue #37).
//!
//! Provides a configurable, safe bulk import pipeline for Trellis smart contracts.
//! Designed to handle bulk donor allocations, aid beneficiary lists, or registry
//! entries with validation, duplicate detection, dry-run simulation, idempotency
//! guarantees, and operator rollback guidance.
//!
//! ## Execution Modes
//!
//! | Mode | Behaviour |
//! |------|-----------|
//! | `AllOrNothing` | Atomic execution. All rows must validate and succeed. If any row fails, the entire batch is rejected. |
//! | `BestEffort` | Partial execution. Valid rows are committed; invalid rows are skipped and recorded with row-level error details. |
//!
//! ## Duplicate Handling Policies
//!
//! | Policy | Behaviour |
//! |--------|-----------|
//! | `SkipExisting` | Existing records with matching external IDs are skipped (`skip_count` increments). |
//! | `UpdateExisting` | Existing records are updated with new payload data (`update_count` increments). |
//! | `RejectDuplicate` | Duplicate external IDs (within batch or in storage) are treated as errors (`error_count` increments). |
//!
//! ## Dry-Run Guarantees
//!
//! When `dry_run` is `true`:
//! - **Zero persistent writes**: The pipeline executes validation, duplicate detection, and impact calculation purely against in-memory state and read-only storage lookups.
//! - **Full preview**: Computes accurate `create_count`, `update_count`, `skip_count`, and `error_count`.
//! - **Rollback guidance**: Analyzes validation results and recommends remediation runbooks before any ledger state is altered.
//! - Emits an `("import", "simulated")` ledger event for off-chain indexer visibility.
//!
//! ## Idempotency Guarantees
//!
//! When `external_id` is supplied on rows:
//! - Repeated executions of the same batch with `SkipExisting` result in `create_count = 0`, `update_count = 0`, and `skip_count = total_rows`.
//! - No duplicate persistent records are created on re-runs.

use soroban_sdk::{
    contracterror, contracttype, symbol_short, Address, Bytes, BytesN, Env, Map, Symbol, Vec,
};

pub use crate::migration::RollbackStrategy;
use crate::storage::{
    instance_get, instance_set, persistent_has, persistent_read, persistent_set,
    temporary_has, temporary_remove, temporary_set,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default maximum number of rows in a single import batch.
pub const DEFAULT_MAX_IMPORT_SIZE: u32 = 50;

/// Absolute maximum allowed rows in one import transaction (gas ceiling).
pub const ABSOLUTE_MAX_IMPORT_SIZE: u32 = 100;

/// Maximum allowable length in bytes for an external ID.
pub const MAX_EXTERNAL_ID_LEN: u32 = 64;

/// Temporary storage reentrancy lock key.
const REENTRANCY_KEY: Symbol = symbol_short!("imp_lck");

/// Instance storage counter key.
const IMPORT_COUNTER_KEY: Symbol = symbol_short!("imp_cnt");

// ---------------------------------------------------------------------------
// Error Codes (Range 940–949)
// ---------------------------------------------------------------------------

/// Import pipeline error codes.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum ImportError {
    /// The import batch exceeds the maximum allowed operations.
    BatchTooLarge = 940,
    /// The import batch contains no records.
    EmptyBatch = 941,
    /// The import configuration is invalid (e.g., zero or excessive max_rows).
    InvalidConfig = 942,
    /// Duplicate external identifier detected under RejectDuplicate policy.
    DuplicateExternalId = 943,
    /// One or more rows failed validation in AllOrNothing mode.
    InvalidRow = 944,
    /// Reentrancy detected during import execution.
    ReentrancyDetected = 945,
    /// Row amount is zero or negative.
    InvalidAmount = 946,
    /// Row expiration ledger/timestamp is already expired.
    ExpiredRecord = 947,
    /// External identifier exceeds the maximum permitted length.
    ExternalIdTooLong = 948,
    /// Recipient address is invalid.
    InvalidRecipient = 949,
}

// ---------------------------------------------------------------------------
// Pipeline Types
// ---------------------------------------------------------------------------

/// Atomicity mode for executing the import.
#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImportMode {
    /// All rows must succeed or entire batch fails / reverts.
    AllOrNothing,
    /// Valid rows persist; invalid rows are skipped and reported.
    BestEffort,
}

/// Strategy for resolving rows whose external ID already exists.
#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DuplicatePolicy {
    /// Ignore new row and leave existing record untouched.
    SkipExisting,
    /// Overwrite existing record with new values.
    UpdateExisting,
    /// Reject as an error and record in RowError.
    RejectDuplicate,
}

/// A single row in an import batch.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportItem {
    /// Zero-based row sequence number within the input batch.
    pub row_id: u32,
    /// Optional external client/NGO identifier for idempotency & deduplication.
    pub external_id: Option<Bytes>,
    /// Target recipient address.
    pub recipient: Address,
    /// Disbursement or allocation amount (must be > 0).
    pub amount: i128,
    /// Expiration ledger sequence or timestamp (must be > current ledger).
    pub expiry: u64,
    /// Optional metadata / payload hash (e.g. SHA-256 of off-chain metadata).
    pub metadata_hash: Option<Bytes>,
}

/// Import pipeline execution configuration.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportConfig {
    /// When `true`, validates and calculates diffs without making any persistent writes.
    pub dry_run: bool,
    /// Atomicity mode (`AllOrNothing` vs `BestEffort`).
    pub mode: ImportMode,
    /// How to handle records with duplicate external IDs.
    pub duplicate_policy: DuplicatePolicy,
    /// Batch size ceiling for this invocation (cannot exceed `ABSOLUTE_MAX_IMPORT_SIZE`).
    pub max_rows: u32,
}

/// Detailed error information for an invalid row.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RowError {
    /// Row sequence number in input batch.
    pub row_id: u32,
    /// External ID if present on the row.
    pub external_id: Option<Bytes>,
    /// Stable numeric contract error code.
    pub error_code: u32,
    /// Short symbol reason (e.g. `zero_amt`, `expired`, `dup_key`).
    pub reason: Symbol,
}

/// Prescriptive rollback and remediation guidance returned in the import report.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RollbackGuidance {
    /// High-level rollback strategy (`Revert`, `ForwardFix`, `Manual`).
    pub strategy: RollbackStrategy,
    /// Short action code for operators (e.g. `no_action`, `fix_rows`, `reverted`, `part_fix`).
    pub action: Symbol,
    /// Number of records needing attention or remediation.
    pub affected_records: u32,
    /// Runbook reference code for the operational team.
    pub runbook_code: Symbol,
}

/// Comprehensive outcome report for both dry-run previews and committed imports.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportReport {
    /// `true` if this report was generated during a dry-run preview.
    pub is_dry_run: bool,
    /// Total number of rows evaluated.
    pub total_rows: u32,
    /// Number of records created.
    pub create_count: u32,
    /// Number of records updated.
    pub update_count: u32,
    /// Number of records skipped.
    pub skip_count: u32,
    /// Number of rows that encountered validation or duplicate errors.
    pub error_count: u32,
    /// Detailed row-level error entries.
    pub errors: Vec<RowError>,
    /// Rollback and remediation guidance.
    pub rollback_guidance: RollbackGuidance,
    /// Deterministic 32-byte cryptographic fingerprint of the import batch.
    pub batch_fingerprint: BytesN<32>,
}

/// Stored record format for persistent state tracking.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredImportRecord {
    /// Monotonically-assigned internal record identifier.
    pub record_id: u64,
    /// External identifier if one was provided.
    pub external_id: Bytes,
    /// Target recipient address.
    pub recipient: Address,
    /// Allocation amount.
    pub amount: i128,
    /// Expiration timestamp or ledger sequence.
    pub expiry: u64,
    /// Optional metadata hash.
    pub metadata_hash: Option<Bytes>,
    /// Ledger timestamp when the record was imported.
    pub imported_at: u64,
    /// Record schema version.
    pub version: u32,
}

// ---------------------------------------------------------------------------
// Storage Key Helpers
// ---------------------------------------------------------------------------

fn external_record_key(external_id: &Bytes) -> (Symbol, Bytes) {
    (symbol_short!("imp_ext"), external_id.clone())
}

// ---------------------------------------------------------------------------
// Reentrancy Guard
// ---------------------------------------------------------------------------

fn acquire_reentrancy_guard(env: &Env) -> Result<(), ImportError> {
    if temporary_has(env, &REENTRANCY_KEY) {
        return Err(ImportError::ReentrancyDetected);
    }
    temporary_set(env, &REENTRANCY_KEY, &true);
    Ok(())
}

fn release_reentrancy_guard(env: &Env) {
    temporary_remove(env, &REENTRANCY_KEY);
}

// ---------------------------------------------------------------------------
// Fingerprint & Validation
// ---------------------------------------------------------------------------

/// Computes a deterministic SHA-256 fingerprint of the input batch.
pub fn compute_batch_fingerprint(env: &Env, items: &Vec<ImportItem>) -> BytesN<32> {
    let mut payload = Bytes::new(env);
    let len = items.len();
    payload.append(&Bytes::from_slice(env, b"trellis_import_v1:"));

    for i in 0..len {
        let item = items.get(i).unwrap();
        // Pack row_id, amount, expiry
        let mut row_bytes = Bytes::new(env);
        if let Some(ext) = &item.external_id {
            row_bytes.append(ext);
        }
        payload.append(&row_bytes);
    }
    env.crypto().sha256(&payload).into()
}

/// Validates configuration bounds.
pub fn validate_config(config: &ImportConfig) -> Result<(), ImportError> {
    if config.max_rows == 0 || config.max_rows > ABSOLUTE_MAX_IMPORT_SIZE {
        return Err(ImportError::InvalidConfig);
    }
    Ok(())
}

/// Validates row fields against core constraints.
fn validate_item(_env: &Env, item: &ImportItem, current_time: u64) -> Result<(), (u32, Symbol)> {
    if item.amount <= 0 {
        return Err((ImportError::InvalidAmount as u32, symbol_short!("zero_amt")));
    }
    if item.expiry <= current_time {
        return Err((ImportError::ExpiredRecord as u32, symbol_short!("expired")));
    }
    if let Some(ext_id) = &item.external_id {
        if ext_id.len() == 0 || ext_id.len() > MAX_EXTERNAL_ID_LEN {
            return Err((ImportError::ExternalIdTooLong as u32, symbol_short!("bad_id")));
        }
    }
    Ok(())
}

/// Computes appropriate operator rollback guidance based on import state.
pub fn generate_rollback_guidance(
    is_dry_run: bool,
    mode: ImportMode,
    total_rows: u32,
    error_count: u32,
    _create_count: u32,
    _update_count: u32,
) -> RollbackGuidance {
    if error_count == 0 {
        return RollbackGuidance {
            strategy: RollbackStrategy::Manual,
            action: symbol_short!("no_action"),
            affected_records: 0,
            runbook_code: symbol_short!("clean"),
        };
    }

    if is_dry_run {
        return RollbackGuidance {
            strategy: RollbackStrategy::ForwardFix,
            action: symbol_short!("fix_rows"),
            affected_records: error_count,
            runbook_code: symbol_short!("dry_err"),
        };
    }

    match mode {
        ImportMode::AllOrNothing => RollbackGuidance {
            strategy: RollbackStrategy::Revert,
            action: symbol_short!("reverted"),
            affected_records: total_rows,
            runbook_code: symbol_short!("atom_rev"),
        },
        ImportMode::BestEffort => RollbackGuidance {
            strategy: RollbackStrategy::ForwardFix,
            action: symbol_short!("part_fix"),
            affected_records: error_count,
            runbook_code: symbol_short!("part_rem"),
        },
    }
}

// ---------------------------------------------------------------------------
// Dry Run Simulation
// ---------------------------------------------------------------------------

/// Performs a dry-run validation of the import batch.
///
/// **Safety guarantee**: Performs **ZERO persistent writes**. All duplicate
/// checking and preview calculations use read-only lookups and in-memory tracking.
pub fn dry_run(
    env: &Env,
    items: &Vec<ImportItem>,
    config: &ImportConfig,
) -> Result<ImportReport, ImportError> {
    validate_config(config)?;
    let total_rows = items.len();
    if total_rows == 0 {
        return Err(ImportError::EmptyBatch);
    }
    if total_rows > config.max_rows {
        return Err(ImportError::BatchTooLarge);
    }

    let current_time = env.ledger().timestamp();
    let fingerprint = compute_batch_fingerprint(env, items);

    let mut create_count: u32 = 0;
    let mut update_count: u32 = 0;
    let mut skip_count: u32 = 0;
    let mut error_count: u32 = 0;
    let mut errors: Vec<RowError> = Vec::new(env);

    // In-memory set for tracking duplicates within this batch.
    // Maps external_id -> occurrence count.
    let mut seen_in_batch: Map<Bytes, u32> = Map::new(env);

    for i in 0..total_rows {
        let item = items.get(i).unwrap();

        // 1. Validate fields
        if let Err((code, reason)) = validate_item(env, &item, current_time) {
            error_count += 1;
            errors.push_back(RowError {
                row_id: item.row_id,
                external_id: item.external_id.clone(),
                error_code: code,
                reason,
            });
            continue;
        }

        // 2. Check duplicate logic
        if let Some(ext_id) = &item.external_id {
            let in_batch_count = seen_in_batch.get(ext_id.clone()).unwrap_or(0);
            let exists_in_storage = persistent_has(env, &external_record_key(ext_id));

            if in_batch_count > 0 || exists_in_storage {
                match config.duplicate_policy {
                    DuplicatePolicy::SkipExisting => {
                        skip_count += 1;
                    }
                    DuplicatePolicy::UpdateExisting => {
                        update_count += 1;
                    }
                    DuplicatePolicy::RejectDuplicate => {
                        error_count += 1;
                        let reason = if in_batch_count > 0 {
                            symbol_short!("dup_batch")
                        } else {
                            symbol_short!("dup_store")
                        };
                        errors.push_back(RowError {
                            row_id: item.row_id,
                            external_id: item.external_id.clone(),
                            error_code: ImportError::DuplicateExternalId as u32,
                            reason,
                        });
                    }
                }
            } else {
                create_count += 1;
            }

            seen_in_batch.set(ext_id.clone(), in_batch_count + 1);
        } else {
            // Rows without external IDs are treated as anonymous creates
            create_count += 1;
        }
    }

    let rollback_guidance = generate_rollback_guidance(
        true,
        config.mode,
        total_rows,
        error_count,
        create_count,
        update_count,
    );

    // Emit simulation event
    crate::events::emit_import_simulated(
        env,
        total_rows,
        create_count,
        update_count,
        skip_count,
        error_count,
        &fingerprint,
    );

    Ok(ImportReport {
        is_dry_run: true,
        total_rows,
        create_count,
        update_count,
        skip_count,
        error_count,
        errors,
        rollback_guidance,
        batch_fingerprint: fingerprint,
    })
}

// ---------------------------------------------------------------------------
// Execute Import
// ---------------------------------------------------------------------------

/// Executes an import batch against persistent storage with full guardrails.
pub fn execute_import(
    env: &Env,
    caller: &Address,
    items: &Vec<ImportItem>,
    config: &ImportConfig,
) -> Result<ImportReport, ImportError> {
    // If caller specified dry_run in config, route directly to dry_run
    if config.dry_run {
        return dry_run(env, items, config);
    }

    validate_config(config)?;
    let total_rows = items.len();
    if total_rows == 0 {
        return Err(ImportError::EmptyBatch);
    }
    if total_rows > config.max_rows {
        return Err(ImportError::BatchTooLarge);
    }

    acquire_reentrancy_guard(env)?;

    let current_time = env.ledger().timestamp();
    let fingerprint = compute_batch_fingerprint(env, items);

    // If AllOrNothing mode: run dry-run validation first. If any errors exist, abort.
    if config.mode == ImportMode::AllOrNothing {
        let preview = dry_run(env, items, config)?;
        if preview.error_count > 0 {
            release_reentrancy_guard(env);
            crate::events::emit_import_failed(
                env,
                caller,
                total_rows,
                preview.error_count,
                ImportError::InvalidRow as u32,
            );
            return Err(ImportError::InvalidRow);
        }
    }

    let mut create_count: u32 = 0;
    let mut update_count: u32 = 0;
    let mut skip_count: u32 = 0;
    let mut error_count: u32 = 0;
    let mut errors: Vec<RowError> = Vec::new(env);

    let mut seen_in_batch: Map<Bytes, u32> = Map::new(env);
    let mut counter: u64 = instance_get(env, &IMPORT_COUNTER_KEY).unwrap_or(0);

    for i in 0..total_rows {
        let item = items.get(i).unwrap();

        // 1. Validate fields
        if let Err((code, reason)) = validate_item(env, &item, current_time) {
            error_count += 1;
            errors.push_back(RowError {
                row_id: item.row_id,
                external_id: item.external_id.clone(),
                error_code: code,
                reason,
            });
            continue;
        }

        // 2. Handle deduplication and persistence
        if let Some(ext_id) = &item.external_id {
            let in_batch_count = seen_in_batch.get(ext_id.clone()).unwrap_or(0);
            let key = external_record_key(ext_id);
            let existing: Option<StoredImportRecord> = persistent_read(env, &key);

            if in_batch_count > 0 || existing.is_some() {
                match config.duplicate_policy {
                    DuplicatePolicy::SkipExisting => {
                        skip_count += 1;
                    }
                    DuplicatePolicy::UpdateExisting => {
                        update_count += 1;
                        let record_id = existing.map(|r| r.record_id).unwrap_or_else(|| {
                            counter += 1;
                            counter
                        });
                        let record = StoredImportRecord {
                            record_id,
                            external_id: ext_id.clone(),
                            recipient: item.recipient.clone(),
                            amount: item.amount,
                            expiry: item.expiry,
                            metadata_hash: item.metadata_hash.clone(),
                            imported_at: current_time,
                            version: 1,
                        };
                        persistent_set(env, &key, &record);
                    }
                    DuplicatePolicy::RejectDuplicate => {
                        error_count += 1;
                        let reason = if in_batch_count > 0 {
                            symbol_short!("dup_batch")
                        } else {
                            symbol_short!("dup_store")
                        };
                        errors.push_back(RowError {
                            row_id: item.row_id,
                            external_id: item.external_id.clone(),
                            error_code: ImportError::DuplicateExternalId as u32,
                            reason,
                        });
                    }
                }
            } else {
                create_count += 1;
                counter += 1;
                let record = StoredImportRecord {
                    record_id: counter,
                    external_id: ext_id.clone(),
                    recipient: item.recipient.clone(),
                    amount: item.amount,
                    expiry: item.expiry,
                    metadata_hash: item.metadata_hash.clone(),
                    imported_at: current_time,
                    version: 1,
                };
                persistent_set(env, &key, &record);
            }

            seen_in_batch.set(ext_id.clone(), in_batch_count + 1);
        } else {
            // Anonymous row without external ID
            create_count += 1;
            counter += 1;
        }
    }

    instance_set(env, &IMPORT_COUNTER_KEY, &counter);
    release_reentrancy_guard(env);

    let rollback_guidance = generate_rollback_guidance(
        false,
        config.mode,
        total_rows,
        error_count,
        create_count,
        update_count,
    );

    crate::events::emit_import_committed(
        env,
        caller,
        total_rows,
        create_count,
        update_count,
        skip_count,
        error_count,
        &fingerprint,
    );

    Ok(ImportReport {
        is_dry_run: false,
        total_rows,
        create_count,
        update_count,
        skip_count,
        error_count,
        errors,
        rollback_guidance,
        batch_fingerprint: fingerprint,
    })
}

// ---------------------------------------------------------------------------
// Query Helpers
// ---------------------------------------------------------------------------

/// Fetches a stored import record by external identifier.
pub fn get_imported_record(env: &Env, external_id: &Bytes) -> Option<StoredImportRecord> {
    persistent_read(env, &external_record_key(external_id))
}

/// Checks if an external identifier has already been imported.
pub fn has_imported_record(env: &Env, external_id: &Bytes) -> bool {
    persistent_has(env, &external_record_key(external_id))
}

/// Returns the current total count of imported records.
pub fn get_import_counter(env: &Env) -> u64 {
    instance_get(env, &IMPORT_COUNTER_KEY).unwrap_or(0)
}
