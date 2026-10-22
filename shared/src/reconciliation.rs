use soroban_sdk::{contracttype, contracterror, Env, String, Vec};

/// Represents a state record as observed by a specific source
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceRecord {
    pub id: u64,
    pub amount: i128,
    pub timestamp: u64,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DriftType {
    MissingRecord,
    DuplicateRecord,
    StaleRecord,
    InconsistentBalance,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DriftItem {
    pub record_id: u64,
    pub drift_type: DriftType,
    pub description: String,
    pub repair_guidance: String,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReconciliationReport {
    pub total_records_scanned: u32,
    pub drifts_detected: Vec<DriftItem>,
}

/// Deterministic key used to identify a candidate duplicate group.
/// Two records are considered candidate duplicates when they share the same
/// `id` and `amount`. The `timestamp` is intentionally excluded so that
/// records that differ only by observation time are still grouped together.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DuplicateCandidate {
    pub record_id: u64,
    pub amount: i128,
    pub occurrences: u32,
    pub earliest_timestamp: u64,
    pub latest_timestamp: u64,
    /// Deterministic fingerprint of the candidate group, derived from
    /// `(record_id, amount)` only. Stable across runs and orderings.
    pub fingerprint: u64,
}

/// Lifecycle state of a merge review action.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MergeReviewState {
    Pending,
    Approved,
    Rejected,
    Applied,
    Reverted,
}

/// A reviewable, auditable merge action for a duplicate candidate.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MergeReviewAction {
    pub action_id: u64,
    pub candidate: DuplicateCandidate,
    pub state: MergeReviewState,
    pub reviewer: String,
    pub reason: String,
    /// Snapshot of the records that would be collapsed by this merge.
    /// Retained so the action can be reversed (reverted) after application.
    pub affected_records: Vec<SourceRecord>,
    pub created_at: u64,
    pub updated_at: u64,
}

/// Errors surfaced by the merge review workflow.
#[contracterror]
#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum MergeReviewError {
    /// The referenced action id does not exist.
    UnknownAction = 1,
    /// The action is not in a state that permits the requested transition.
    InvalidTransition = 2,
    /// The candidate group no longer matches the underlying records.
    StaleCandidate = 3,
}

/// Deterministic fingerprint for a duplicate candidate group.
/// Uses a fixed FNV-1a style mix over `(record_id, amount)` so that the same
/// inputs always produce the same fingerprint regardless of iteration order.
pub fn duplicate_fingerprint(record_id: u64, amount: i128) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    let mut id = record_id;
    let mut i = 0;
    while i < 8 {
        hash ^= (id & 0xff) as u64;
        hash = hash.wrapping_mul(0x100000001b3);
        id >>= 8;
        i += 1;
    }
    let mut amt = amount as u128;
    let mut j = 0;
    while j < 16 {
        hash ^= (amt & 0xff) as u64;
        hash = hash.wrapping_mul(0x100000001b3);
        amt >>= 8;
        j += 1;
    }
    hash
}

/// Detects candidate duplicates deterministically across a single source.
/// Records are grouped by `(id, amount)`; any group with more than one
/// occurrence is a candidate. Output is sorted by `fingerprint` ascending so
/// that the result is reproducible regardless of input ordering.
pub fn detect_duplicate_candidates(env: &Env, records: Vec<SourceRecord>) -> Vec<DuplicateCandidate> {
    let mut candidates: Vec<DuplicateCandidate> = Vec::new(env);
    let mut i = 0;
    while i < records.len() {
        let rec = records.get(i).unwrap();
        let fp = duplicate_fingerprint(rec.id, rec.amount);

        // Skip if this fingerprint has already been emitted.
        let mut already_emitted = false;
        let mut c = 0;
        while c < candidates.len() {
            if candidates.get(c).unwrap().fingerprint == fp {
                already_emitted = true;
                break;
            }
            c += 1;
        }
        if already_emitted {
            i += 1;
            continue;
        }

        let mut occurrences: u32 = 0;
        let mut earliest: u64 = rec.timestamp;
        let mut latest: u64 = rec.timestamp;
        let mut j = 0;
        while j < records.len() {
            let other = records.get(j).unwrap();
            if other.id == rec.id && other.amount == rec.amount {
                occurrences += 1;
                if other.timestamp < earliest {
                    earliest = other.timestamp;
                }
                if other.timestamp > latest {
                    latest = other.timestamp;
                }
            }
            j += 1;
        }

        if occurrences > 1 {
            candidates.push_back(DuplicateCandidate {
                record_id: rec.id,
                amount: rec.amount,
                occurrences,
                earliest_timestamp: earliest,
                latest_timestamp: latest,
                fingerprint: fp,
            });
        }
        i += 1;
    }

    // Insertion sort by fingerprint for determinism (Vec has no sort in Soroban).
    let mut a = 1;
    while a < candidates.len() {
        let key = candidates.get(a).unwrap();
        let mut b = a;
        while b > 0 {
            let prev = candidates.get(b - 1).unwrap();
            if prev.fingerprint <= key.fingerprint {
                break;
            }
            candidates.set(b, prev);
            b -= 1;
        }
        candidates.set(b, key);
        a += 1;
    }

    candidates
}

/// Runs a dry-run reconciliation across ledger, database, and user-facing records.
/// Checks for invariants and returns a report with detected drift and repair guidance.
pub fn run_reconciliation(
    env: &Env,
    ledger_records: Vec<SourceRecord>,
    db_records: Vec<SourceRecord>,
    user_records: Vec<SourceRecord>,
    current_timestamp: u64,
) -> ReconciliationReport {
    let mut drifts = Vec::new(env);
    let mut total_scanned = 0;
    
    // A simple O(N^2) or linear check approach. Since Soroban Vec doesn't have a HashMap, we'll iterate.
    // For large lists, an off-chain indexer would use a real db, but for contract-level / dry-run logic:
    
    // Check Ledger against DB and User
    let mut i = 0;
    while i < ledger_records.len() {
        let l_rec = ledger_records.get(i).unwrap();
        total_scanned += 1;
        
        let mut found_in_db = false;
        let mut db_rec_match = l_rec.clone();
        
        let mut db_count = 0;
        let mut j = 0;
        while j < db_records.len() {
            let d_rec = db_records.get(j).unwrap();
            if d_rec.id == l_rec.id {
                found_in_db = true;
                db_rec_match = d_rec;
                db_count += 1;
            }
            j += 1;
        }
        
        if db_count > 1 {
            drifts.push_back(DriftItem {
                record_id: l_rec.id,
                drift_type: DriftType::DuplicateRecord,
                description: String::from_str(env, "Record duplicated in database"),
                repair_guidance: String::from_str(env, "Remove duplicates in DB keeping the latest timestamp"),
            });
        }
        
        if !found_in_db {
            drifts.push_back(DriftItem {
                record_id: l_rec.id,
                drift_type: DriftType::MissingRecord,
                description: String::from_str(env, "Ledger record missing in database"),
                repair_guidance: String::from_str(env, "Re-sync database from ledger events"),
            });
        } else {
            // Check balance consistency
            if db_rec_match.amount != l_rec.amount {
                drifts.push_back(DriftItem {
                    record_id: l_rec.id,
                    drift_type: DriftType::InconsistentBalance,
                    description: String::from_str(env, "Database amount does not match ledger"),
                    repair_guidance: String::from_str(env, "Update database amount to match ledger canonical state"),
                });
            }
            
            // Check stale record (e.g. lag > 1000 units)
            if l_rec.timestamp > db_rec_match.timestamp + 1000 {
                drifts.push_back(DriftItem {
                    record_id: l_rec.id,
                    drift_type: DriftType::StaleRecord,
                    description: String::from_str(env, "Database record is stale compared to ledger"),
                    repair_guidance: String::from_str(env, "Trigger a fast-forward sync for this record"),
                });
            }
        }
        
        // Similarly for user records
        let mut found_in_user = false;
        let mut user_rec_match = l_rec.clone();
        
        let mut k = 0;
        while k < user_records.len() {
            let u_rec = user_records.get(k).unwrap();
            if u_rec.id == l_rec.id {
                found_in_user = true;
                user_rec_match = u_rec;
            }
            k += 1;
        }
        
        if !found_in_user {
            // Might be okay if user hasn't refreshed, but flag as missing in cache
            drifts.push_back(DriftItem {
                record_id: l_rec.id,
                drift_type: DriftType::MissingRecord,
                description: String::from_str(env, "Ledger record missing in user view"),
                repair_guidance: String::from_str(env, "Invalidate user cache for this record"),
            });
        } else if user_rec_match.amount != l_rec.amount {
            drifts.push_back(DriftItem {
                record_id: l_rec.id,
                drift_type: DriftType::InconsistentBalance,
                description: String::from_str(env, "User view amount does not match ledger"),
                repair_guidance: String::from_str(env, "Force client refresh of balances"),
            });
        }
        
        i += 1;
    }
    
    // Also check if there are DB records not in ledger
    let mut j = 0;
    while j < db_records.len() {
        let d_rec = db_records.get(j).unwrap();
        let mut found_in_ledger = false;
        
        let mut i = 0;
        while i < ledger_records.len() {
            if ledger_records.get(i).unwrap().id == d_rec.id {
                found_in_ledger = true;
                break;
            }
            i += 1;
        }
        
        if !found_in_ledger {
            drifts.push_back(DriftItem {
                record_id: d_rec.id,
                drift_type: DriftType::MissingRecord,
                description: String::from_str(env, "Database record missing in ledger (orphan)"),
                repair_guidance: String::from_str(env, "Archive or delete orphan database record"),
            });
            total_scanned += 1;
        }
        
        j += 1;
    }

    ReconciliationReport {
        total_records_scanned: total_scanned,
        drifts_detected: drifts,
    }
}
