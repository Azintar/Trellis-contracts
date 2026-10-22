use crate::errors::Error;
use crate::replay::{consume_payload, SignedPayload};
use crate::serialization::canonical_bytes;
use soroban_sdk::{
    testutils::{Address as _, BytesN as _, Ledger as _},
    Address, BytesN, Env, Symbol,
};

#[test]
fn test_consume_payload_success() {
    let env = Env::default();
    env.mock_all_auths();
    let signer = Address::generate(&env);
    let nonce = BytesN::random(&env);
    let scope = Symbol::new(&env, "test_scope");

    // set time to 100
    env.ledger().set_timestamp(100);

    let payload = SignedPayload {
        nonce: nonce.clone(),
        expiry: 200,
        scope: scope.clone(),
    };

    let res = consume_payload(&env, &signer, &payload, &scope);
    assert_eq!(res, Ok(()));
}

#[test]
fn test_canonical_serialization_stable_field_order() {
    let env = Env::default();
    let nonce = BytesN::random(&env);
    let scope = Symbol::new(&env, "test_scope");

    let payload = SignedPayload {
        nonce: nonce.clone(),
        expiry: 200,
        scope: scope.clone(),
    };

    let bytes1 = canonical_bytes(&env, &payload);
    let bytes2 = canonical_bytes(&env, &payload);
    assert_eq!(bytes1, bytes2);
}

#[test]
fn test_canonical_serialization_equivalent_payloads() {
    let env = Env::default();
    let nonce = BytesN::random(&env);
    let scope = Symbol::new(&env, "test_scope");

    let payload_a = SignedPayload {
        nonce: nonce.clone(),
        expiry: 200,
        scope: scope.clone(),
    };
    let payload_b = SignedPayload {
        nonce: nonce.clone(),
        expiry: 200,
        scope: scope.clone(),
    };

    assert_eq!(
        canonical_bytes(&env, &payload_a),
        canonical_bytes(&env, &payload_b)
    );
}

#[test]
fn test_canonical_serialization_distinguishes_fields() {
    let env = Env::default();
    let nonce = BytesN::random(&env);
    let scope = Symbol::new(&env, "test_scope");

    let payload_a = SignedPayload {
        nonce: nonce.clone(),
        expiry: 200,
        scope: scope.clone(),
    };
    let payload_b = SignedPayload {
        nonce: nonce.clone(),
        expiry: 201,
        scope: scope.clone(),
    };

    assert_ne!(
        canonical_bytes(&env, &payload_a),
        canonical_bytes(&env, &payload_b)
    );
}

#[test]
fn test_consume_payload_replay() {
    let env = Env::default();
    env.mock_all_auths();
    let signer = Address::generate(&env);
    let nonce = BytesN::random(&env);
    let scope = Symbol::new(&env, "test_scope");

    env.ledger().set_timestamp(100);

    let payload = SignedPayload {
        nonce: nonce.clone(),
        expiry: 200,
        scope: scope.clone(),
    };

    let res1 = consume_payload(&env, &signer, &payload, &scope);
    assert_eq!(res1, Ok(()));

    // Replay attempt
    let res2 = consume_payload(&env, &signer, &payload, &scope);
    assert_eq!(res2, Err(Error::AlreadyClaimed));
}

#[test]
fn test_consume_payload_expired() {
    let env = Env::default();
    env.mock_all_auths();
    let signer = Address::generate(&env);
    let nonce = BytesN::random(&env);
    let scope = Symbol::new(&env, "test_scope");

    env.ledger().set_timestamp(300);

    let payload = SignedPayload {
        nonce: nonce.clone(),
        expiry: 200,
        scope: scope.clone(),
    };

    let res = consume_payload(&env, &signer, &payload, &scope);
    assert_eq!(res, Err(Error::Expired));
}

#[test]
fn test_consume_payload_wrong_scope() {
    let env = Env::default();
    env.mock_all_auths();
    let signer = Address::generate(&env);
    let nonce = BytesN::random(&env);
    let scope = Symbol::new(&env, "test_scope");
    let wrong_scope = Symbol::new(&env, "wrong");

    env.ledger().set_timestamp(100);

    let payload = SignedPayload {
        nonce: nonce.clone(),
        expiry: 200,
        scope: scope.clone(),
    };

    let res = consume_payload(&env, &signer, &payload, &wrong_scope);
    assert_eq!(res, Err(Error::InvalidArgument));
}
