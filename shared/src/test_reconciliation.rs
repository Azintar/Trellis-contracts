use soroban_sdk::{Env, Vec};
use crate::reconciliation::{run_reconciliation, SourceRecord, DriftType};

#[test]
fn test_reconciliation_scenarios() {
    let env = Env::default();

    // Setup records
    let mut ledger_records = Vec::new(&env);
    let mut db_records = Vec::new(&env);
    let mut user_records = Vec::new(&env);

    let current_timestamp = 10000;

    // 1. Missing Record
    // Ledger has id:1, DB is missing it
    ledger_records.push_back(SourceRecord { id: 1, amount: 100, timestamp: current_timestamp });

    // 2. Inconsistent Balance
    // Ledger has id:2 amount: 200, DD has id:2 amount: 150
    ledger_records.push_back(SourceRecord { id: 2, amount: 200, timestamp: current_timestamp });
    db_records.push_back(SourceRecord { id: 2, amount: 150, timestamp: current_timestamp });

    // 3. Stale Record
    // Ledger has id:3 timestamp: 5000, DB has id:3 timestamp: 3000 (diff > 1000)
    ledger_records.push_back(SourceRecord { id: 3, amount: 300, timestamp: 5000 });
    db_records.push_back(SourceRecord { id: 3, amount: 300, timestamp: 3000 });

    // 4. Duplicate Record in DB
    // DE has two records with id:4
    ledger_records.push_back(SourceRecord { id: 4, amount: 400, timestamp: current_timestamp });
    db_records.push_back(SourceRecord { id: 4, amount: 400, timestamp: current_timestamp });
    db_records.push_back(SourceRecord { id: 4, amount: 400, timestamp: current_timestamp });

    // 5. Orphan DB record (not in ledger)
    db_records.push_back(SourceRecord { id: 5, amount: 500, timestamp: current_timestamp });

    // Populate user_records with some values to prevent missing record drifts for all of them
    user_records.push_back(SourceRecord { id: 1, amount: 100, timestamp: current_timestamp });
    user_records.push_back(SourceRecord { id: 2, amount: 200, timestamp: current_timestamp });
    user_records.push_back(SourceRecord { id: 3, amount: 300, timestamp: current_timestamp });
    user_records.push_back(SourceRecord { id: 4, amount: 400, timestamp: current_timestamp });

    let report = run_reconciliation(&env, ledger_records, db_records, user_records, current_timestamp);

    let mut missing = false;
    let mut inconsistent = false;
    let mut stale = false;
    let mut duplicate = false;
    let mut orphan = false;

    for drift in report.drifts_detected {
        match drift.drift_type {
            DriftType::MissingRecord => {
                if drift.record_id == 1 { missing = true; }
                if drift.record_id == 5 { orphan = true; }
            },
            DriftType::InconsistentBalance => {
                if drift.record_id == 2 { inconsistent = true; }
            },
            DriftType::StaleRecord => {
                if drift.record_id == 3 { stale = true; }
            },
            DriftType::DuplicateRecord => {
                if drift.record_id == 4 { duplicate = true; }
            }
        }
    }

    assert!(missing, "Expected MissingRecord drift for id 1");
    assert!(inconsistent, "Expected InconsistentBalance drift for id 2");
    assert!(stale, "Expected StaleRecord drift for id 3");
    assert!(duplicate, "Expected DuplicateRecord drift for id 4");
    assert!(orphan, "Expected MissingRecord drift for id 5 (orphan in DB)");
}
