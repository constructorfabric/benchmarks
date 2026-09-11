//! `OAuth2` access-token cache (ADR-0008).
//!
//! `TinyUfo`, the structure underneath `pingora-memory-cache`, hashes keys to a `u64`
//! and never compares them for equality, so a collision would silently hand one tenant
//! another tenant's token. The cache therefore stores a [`CachedToken`] carrying the key
//! it was filed under and every hit is re-verified before use: a mismatch is treated as a
//! miss and the token is refetched.

use std::time::Duration;

use pingora_memory_cache::MemoryCache;
use sha2::Digest;
use toolkit_auth::SecretString;

/// Safety margin subtracted from an IdP-reported `expires_in` before caching.
pub const EXPIRY_SAFETY_MARGIN: Duration = Duration::from_secs(30);

/// A bearer token together with the cache key it was filed under.
#[derive(Clone)]
pub struct CachedToken {
    pub key: String,
    pub token: SecretString,
}

/// The multi-tenant `OAuth2` token cache.
pub struct TokenCache {
    cache: MemoryCache<String, CachedToken>,
    ttl: Duration,
}

impl TokenCache {
    /// Builds a cache holding at most `capacity` entries for at most `ttl`.
    #[must_use]
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            cache: MemoryCache::new(capacity.max(1)),
            ttl,
        }
    }

    /// Builds a cache from the gear configuration.
    #[must_use]
    pub fn from_config(config: &crate::config::TokenCacheConfig) -> Self {
        Self::new(
            usize::try_from(config.capacity).unwrap_or(usize::MAX),
            Duration::from_secs(config.ttl_secs),
        )
    }

    /// Returns the cached token for `key` when it is still valid.
    ///
    /// A hit whose stored key differs from the lookup key is a collision and is treated
    /// as a miss.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<SecretString> {
        let (entry, _status) = self.cache.get(key);
        let entry = entry?;
        if entry.key != key {
            return None;
        }
        Some(entry.token)
    }

    /// Files a token under `key`.
    ///
    /// A zero or negative lifetime is not cached: the next request retries the `IdP`.
    pub fn put(&self, key: &str, token: SecretString, expires_in: Duration) {
        let Some(ttl) = expires_in.checked_sub(EXPIRY_SAFETY_MARGIN) else {
            return;
        };
        let ttl = ttl.min(self.ttl);
        if ttl.is_zero() {
            return;
        }
        self.cache.put(key, CachedToken { key: key.to_owned(), token }, Some(ttl));
    }
}

/// Deterministic SHA-256 digest of a plugin configuration, used as a cache-key component.
///
/// Keys are sorted so that two bindings carrying the same settings in a different JSON
/// order share an entry.
#[must_use]
pub fn hash_config(config: &serde_json::Map<String, serde_json::Value>) -> String {
    // The binding's map keeps insertion order, so the entries are re-collected into a
    // `BTreeMap` before serializing: two bindings carrying the same settings in a
    // different JSON order then hash alike.
    let sorted: std::collections::BTreeMap<&str, &serde_json::Value> =
        config.iter().map(|(key, value)| (key.as_str(), value)).collect();
    let canonical = serde_json::to_string(&sorted).unwrap_or_default();
    hex::encode(sha2::Sha256::digest(canonical.as_bytes()))
}

#[cfg(test)]
#[path = "token_cache_tests.rs"]
mod tests;
