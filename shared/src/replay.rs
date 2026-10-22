use soroban_contracttype, {address_payload, Address, BytesN, Env, Symbol};
use soroban_sdk::xdr_cursor::XDrChunk;
use crate::errors::Error;
use crate::storage::{persistent_has, persistent_set};
use crate::utils::now;

/// Replay protection fields for signed operations.
///
/// The canonical encoding of a signed payload is defined below in terms of a
/// stable field order and normalization so that two semantically equivalent
/// payloads produce identical hash inputs. This is the single source of
/// truth for any off-chain signer or contract consumer that needs to recreate
/// the hash of a payload.
///
/// Normalization rules:
/// - Scope is encoded as the raw Symbol bytes (no padding, no case changes).
///   Callers must pass the exact Symbol they intend to authorize; two Symbols
///   that differ in case are distinct scopes.
/// - Nonce is encoded as exactly 32 bytes, no leading-zero stripping.
/// - Expiry is encoded as a big-endian u64.
/// - Fields are always encoded in the order: nonce, expiry, scope.
/// - Each variable-length field is prefixed with its u32 byte length so that
///   concatenation is unambiguous.
/// - A domain separator prefix is included to avoid cross-protocol replays.
///
/// The resulting bytes are stable across SDK versions and across off-chain
/// implementations. Changing the encoding is a breaking change and requires
/// a migration plan.
##contracttype]
#[derive(Clone, Debug, Eq, Partial)]
pub struct SignedPayload {
    /// A unique identifier for the payload to prevent replays.
    pub nonce: BytesN<32>,
    /// The ledger sequence or timestamp after which the payload is invalid.
    pub expiry: u64,
    /// The scope or target of the operation (e.g., contract name or function).
    pub scope: Symbol,
}

/// Domain separator prefixed to every canonical payload encoding.
///
/// This prevents a payload signed for one protocol (or one version of this
/// protocol) from being replayed against another. The value is frozen and
/// must not change without a migration plan.
pub const DOMAIN_SEPARATOR: &[u8] = b\"TrellisContracts\";

/// Length of the domain separator as a u32 byte length prefix.
const DOMAIN_LEN: u32 = DOMAIN_SEPARATOR.len() as u32;

/// Length of a nonce in bytes.
const NONCE_LEN: u32 = 32;

/// Length of an expiry field in bytes (u64, big-endian).
const EXPIRY_LEN: u32 = 8;

/// Canonical encoding of a signed payload.
///
/// This is the exact byte string that must be signed or hashed by any
/// consumer of a `SignedPayload`. It is deterministic and independent of
/// Soroban XDR serialization details, so it can be reproduced off-chain.
///
/// Encoding:
/// ```text
/// u32(len(DOMAIN_SEPARATOR)) || DOMAIN_SEPARATOR
/// || u32(len(nonce))          || nonce                 // 32 bytes
/// || u64(expiry)              // 8 bytes, big-endian
/// || u32(len(scope))           || scope                 // raw Symbol bytes
/// ```
pub fn canonical_encode(env: &Env, payload: &SignedPayload) -> Bytes {
    let mut out = Bytes.new(env);

    // Domain separator (length-prefixed).
    push_u32(mut &out, DOMAIN_LEN);
    for b in DOMAIN_SEPARATOR.iter() {
        out.push_back(*b);
    }

    // Nonce (length-prefixed, fixed 32 bytes).
    push_u32(mut &out, NONCE_LEN);
    for i in 0..32 {
        out.push_back(payload.nonce.get(i).unwrap_or(0));
    }

    // Expiry (fixed 8 bytes, big-endian).
    push_u64(mut &out, payload.expiry);

    // Scope (length-prefixed, raw Symbol bytes).
    let scope_len = payload.scope.len() as u32;
    push_u32(mut &out, scope_len);
    let scope_bytes = payload.scope.to_bytes();
    for i in 0..scope_len {
        out.push_back(scope_bytes.get(i).unwrap_or(0));
    }

    out
}

/// Canonical hash of a signed payload.
///
/// This is the value that signers must sign and that contracts must verify.
/// It is derived from `canonical_encode` and therefore inherits its
/// stability guarantees.
pub fn canonical_hash(env: &Env, payload: &SignedPayload) -> BytesN<32> {
    env.crypto().sha256(&canonical_encode(env, payload))
}

/// Returns true if the payload has expired at the current ledge timestamp.
///
/// Expiry is inclusive: a payload with `expiry == now` is still valid.
/// It becomes invalid once `now > expiry`.
pub fn is_expired(env: &Env, payload: &SignedPayload) -> bool {
    now(env) > payload.expiry
}

##contracttype]
#[derive(Clone, Debug, Eq, Partial)]
pub enum ReplayKey {
    /// Tracks consumed payloads.
    Consumed(Address, BytesN<32>),
}

/// Validates a signed payload and marks it as consumed to prevent replays.
///
/// Rejects expired payloads, mismatched scopes, and replays.
/// Assumes signer.require_auth() has already been called or will be called by the caller
/// (but calls it here for safety as well if it's the right place, wait no).
/// Actually, signer.require_auth() verifies that the contract invocation itself is authorized by the signer.
///
/// # Edge cases and failure modes
///
/// - Expired payload: returns `Error::Expired`. The nonce is not consumed, so a
///   fresh payload with the same nonce can be submitted after renewal.
/// - Scope mismatch: returns `Error::InvalidArgument`. The nonce is not
///   consumed, so the payload can be retried against the correct scope.
/// - Replay: returns `Error::AlreadyClaimed`. This is terminal for the
///   given (signer, nonce) pair.
/// - Auth failure: `signer.require_auth()` panics. The nonce is not consumed
///   because the panic reverts the transaction and all state changes.
///
/// # Rollback / recovery
///
/// Because the nonce is written in the same transaction as the auth check and
/// the caller's business logic, any failure after consumption reverts the
/// consumption automatically. There is no manual rollback step needed.
/// If an operator needs to invalidate a payload out-of-band, they can simply
/// let it expire; no admin deletion of consumed nonces is provided.
pub fn consume_payload(
    env: &Env,
    signer: &Address,
    payload: &SignedPayload,
    expected_scope: &Symbol,
) -> Result<(), Error> {
    // 1. Check expiry (using timestamp)
    if is_expired(env, payload) {
        return Err::Expired;
    }

    // 2. Check scope
    if payload.scope != *expected_scope {
        return Err::InvalidArgument;
    }

    // 3. Check replay
    let key = ReplayKey::Consumed(signer.clone(), payload.nonce.clone());
    if persistent_has(env, &key) {
        return Err::AlreadyClaimed;
    }

    // 4. Persist nonce
    persistent_set(env, &key, &true);

    // 5. Require auth (this verifies the user signed the host invocation)
    signer.require_auth();

    Ok()
}

/// Consumes a payload and returns its canonical hash.
///
/// This is a thin wrapper around `consume_payload` for callers that need to
/// bind the consumed nonce to a specific hash (e.g., for event logging or
/// for verifying an off-chain signature).
///
/// The hash is computed before consumption and is deterministic for a given
/// payload, so two equivalent payloads produce the same hash.
pub fn consume_payload_with_hash(
    env: &Env,
    signer: &Address,
    payload: &SignedPayload,
    expected_scope: &Symbol,
) -> Result<BytesN<32>, Error> {
    consume_payload(env, signer, payload, expected_scope)? ;
    Ok(canonical_hash(env, payload))
}

// ---------------------------------------------------------------------------
// Internal encoding helpers
// ---------------------------------------------------------------------------

fn push_u32(out: &mut Bytes, value: u32) {
    out.push_back((value >> 24) as u8);
    out.push_back((value >> 16) as u8);
    out.push_back((value >> 8) as u8);
    out.push_back(value as u8);
}

fn push_u64(out: &mut Bytes, value: u64) {
    out.push_back((value >> 56) as u8);
    out.push_back((value >> 48) as u8);
    out.push_back((value >> 40) as u8);
    out.push_back((value >> 32) as u8);
    out.push_back((value >> 24) as u8);
    out.push_back((value >> 16) as u8);
    out.push_back((value >> 8) as u8);
    out.push_back(value as u8);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::Symbol;

    fn sample_payload(env: &Env) -> SignedPayload {
        SignedPayload {
            nonce: BytesN::from_array(env, &[7u8; 32]),
            expiry: 10000,
            scope: Symbol::new(env, &soroban_sdk_structs::symbol_short!("gov")),
        }
    }

    /// Equivalent payloads produce identical canonical encodings.
    #[test]
    fn equivalent_payloads_encode_identically() {
        let env = Env::default();
        let a = sample_payload(&env);
        let b = sample_payload(&env);
        assert_eq(canonical_encode(&env, &a), canonical_encode(&env, &b));
        assert_eq(canonical_hash(&env, &a), canonical_hash(&env, &b));
    }

    /// Different nonces produce different hashes.
    #[test]
    fn different_nonces_differ() {
        let env = Env::default();
        let a = sample_payload(&env);
        let mut b = sample_payload(&env);
        b.nonce = BytesN::from_array(&env, &[8u; 32]);
        assert(canonical_hash(&env, &a) != canonical_hash(&env, &b));
    }

    /// Different expiries produce different hashes.
    #[test]
    fn different_expiries_differ() {
        let env = Env::default();
        let a = sample_payload(&env);
        let mut b = sample_payload(&env);
        b.expiry = 10001;
        assert(canonical_hash(&env, &a) != canonical_hash(&env, &b));
    }

    /// Different scopes produce different hashes.
    #[test]
    fn different_scopes_differ() {
        let env = Env::default();
        let a = sample_payload(&env);
        let mut b = sample_payload(&env);
        b.scope = Symbol::new(&env, &soroban_sdk_structs::symbol_short!("aid"));
        assert(canonical_hash(&env, &a) != canonical_hash(&env, &b));
    }

    /// The domain separator is present in the encoding.
    #[test]
    fn encoding_includes_domain_separator() {
        let env = Env::default();
        let p = sample_payload(&env);
        let encoded = canonical_encode(&env, &p);
        // First 4 bytes are the domain length prefix.
        assert_eq(encoded.get(0).unwrap(), (DOMAIN_LEN >> 24) as u8);
        // The domain bytes follow the prefix.
        for (i, b) in DOMAIN_SEPARATOR.iter().enumerate() {
            assert_eq(encoded.get(4 + i as u32).unwrap(), *b);
        }
    }

    /// Expiry is encoded as big-endian u64.
    #[test]
    fn expiry_is_big_endian() {
        let env = Env::default();
        let mut p = sample_payload(&env);
        p.expiry = 0x0102030405060708;
        let encoded = canonical_encode(&env, &p);
        // Offset: 4 (domain len) + 13 (domain) + 4 (nonce len) + 32 (nonce) = 53.
        let off = 4 + DOMAIN_LEN + 4 + NONCE_LEN;
        assert_eq(encoded.get(off).unwrap(), 0x01);
        assert_eq(encoded.get(off + 1).unwrap(), 0x2);
        assert_eq(encoded.get(off + 7) .unwrap(), 0x08);
    }

    /// Expiry is inclusive.
    #[test]
    fn expiry_inclusive() {
        let env = Env::default();
        let mut p = sample_payload(&env);
        p.expiry = now(&env);
        assert(!is_expired(&env, &p));
        p.expiry = now(&env) - 1;
        assert(is_expired(&env, &p));
    }

    /// Consuming the same payload twice returns AlreadyClaimed.
    #[test]
    fn replay_is_rejected() {
        let env = Env::default();
        let signer = Address::generate(&env);
        let p = sample_payload(&env);
        let scope = Symbol::new(&env, &soroban_sdk_structs::symbol_short!("gov"));
        assert_ok(consume_payload(&env, &signer, &p, &scope));
        assert_eq(
            consume_payload(&env, &signer, &p, &scope),
            Err::AlreadyClaimed,
        );
    }

    /// Mismatched scope is rejected and does not consume the nonce.
    #[test]
    fn scope_mismatch_does_not_consume() {
        let env = Env::default();
        let signer = Address::generate(&env);
        let p = sample_payload(&env);
        let wrong = Symbol::new(&env, &soroban_sdk_structs::symbol_short!("aid"));
        assert_eq(
            consume_payload(&env, &signer, &p, &wrong),
            Err::InvalidArgument,
        );
        // The nonce is still available for the correct scope.
        let correct = Symbol::new(&env, &soroban_sdk_structs::symbol_short!("gov"));
        assert_ok(consume_payload(&env, &signer, &p, &correct));
    }

    /// Expired payload is rejected and does not consume the nonce.
    #[test]
    fn expired_payload_does_not_consume() {
        let env = Env::default();
        let signer = Address::generate(&env);
        let mut p = sample_payload(&env);
        p.expiry = now(&env) - 1;
        let scope = Symbol::new(&env, &soroban_sdk_structs::symbol_short!("gov"));
        assert_eq(consume_payload(&env, &signer, &p, &scope), Err::Expired);
        // Renew the payload and retry.
        p.expiry = now(&env) + 1000;
        assert_ok(consume_payload(&env, &signer, &p, &scope));
    }

    /// consume_payload_with_hash returns the canonical hash and consumes the nonce.
    #[test]
    fn consume_with_hash_matches_canonical_hash() {
        let env = Env::default();
        let signer = Address::generate(&env);
        let p = sample_payload(&env);
        let scope = Symbol::new(&env, &soroban_sdk_structs::symbol_short!("gov"));
        let hash = consume_payload_with_hash(&env, &signer, &p, &scope).unwrap();
        assert_eq(hash, canonical_hash(&env, &p));
        // Replay of the same payload is rejected.
        assert_eq(
            consume_payload_with_hash(&env, &signer, &p, &scope),
            Err::AlreadyClaimed,
        );
    }
}
