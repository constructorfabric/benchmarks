//! Deterministic hashing of JSON documents.
//!
//! Used for cache keys where structurally equal configuration must produce
//! the same digest regardless of key order.

/// FNV-1a 64-bit offset basis.
const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a 64-bit prime.
const PRIME: u64 = 0x0000_0100_0000_01b3;

/// Fold `bytes` into the FNV-1a `state`.
fn absorb(state: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *state ^= u64::from(*byte);
        *state = state.wrapping_mul(PRIME);
    }
}

/// Stable digest of a JSON value: keys are visited in sorted order and every
/// scalar is tagged with its kind, so `1` and `"1"` never collide.
#[must_use]
pub fn stable_hash(value: &serde_json::Value) -> u64 {
    let mut state = OFFSET;
    match value {
        serde_json::Value::Null => absorb(&mut state, b"null"),
        serde_json::Value::Bool(inner) => absorb(&mut state, inner.to_string().as_bytes()),
        serde_json::Value::Number(inner) => {
            absorb(&mut state, b"#");
            absorb(&mut state, inner.to_string().as_bytes());
        }
        serde_json::Value::String(inner) => {
            absorb(&mut state, b"$");
            absorb(&mut state, inner.as_bytes());
        }
        serde_json::Value::Array(items) => {
            absorb(&mut state, b"[");
            for item in items {
                let digest = stable_hash(item);
                absorb(&mut state, &digest.to_le_bytes());
            }
            absorb(&mut state, b"]");
        }
        serde_json::Value::Object(map) => {
            absorb(&mut state, b"{");
            for (key, item) in map {
                absorb(&mut state, key.as_bytes());
                let digest = stable_hash(item);
                absorb(&mut state, &digest.to_le_bytes());
            }
            absorb(&mut state, b"}");
        }
    }
    state
}

#[cfg(test)]
#[path = "hash_tests.rs"]
mod hash_tests;
