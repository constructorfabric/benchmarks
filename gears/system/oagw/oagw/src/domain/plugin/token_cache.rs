//! The `OAuth2` client-credentials token cache
//! (`cpt-cf-oagw-dod-token-cache`, `cpt-cf-oagw-state-cached-token`).
//!
//! Backed by `pingora_memory_cache::MemoryCache`, per ADR-0008: `TinyUfo`
//! hashes the `String` key to a `u64` and does not use `Eq` for collision
//! resolution, so [`CachedToken`] stores the original lookup key alongside
//! the token and every hit re-verifies it, treating a mismatch as a miss
//! (`cpt-cf-oagw-algo-token-cache-lookup`) rather than ever returning a
//! different key's token.

use std::time::Duration;

use pingora_memory_cache::MemoryCache;
use toolkit_auth::oauth2::SecretString;

/// One cached access token, wrapped with its own lookup key for
/// hash-collision verification on read (`cpt-cf-oagw-state-cached-token`).
#[derive(Clone)]
struct CachedToken {
    key: String,
    bearer: SecretString,
}

/// The `OAuth2` client-credentials token cache
/// (`cpt-cf-oagw-dod-token-cache`). Shared by the `Form` and `Basic`
/// variants; the variant tag is folded into the cache key by the caller, so
/// one cache instance is sufficient (`cpt-cf-oagw-algo-token-cache-lookup`).
pub struct TokenCache {
    cache: MemoryCache<String, CachedToken>,
}

impl TokenCache {
    /// Builds a cache bounded to `capacity` entries (`OagwConfig::token_cache_capacity`).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            cache: MemoryCache::new(capacity.max(1)),
        }
    }

    /// Looks up `key`, returning a live token only when an unexpired entry
    /// exists AND its recorded key matches `key`
    /// (`cpt-cf-oagw-algo-token-cache-lookup`). A key mismatch — the
    /// hash-collision defense — is treated as a miss, never as the
    /// mismatched entry's token.
    // @cpt-begin:cpt-cf-oagw-dod-token-cache:p2:inst-token-cache-get-fn-01
    #[must_use]
    pub fn get(&self, key: &str) -> Option<SecretString> {
        let (entry, _status) = self.cache.get(key);
        entry.and_then(|cached| (cached.key == key).then_some(cached.bearer))
    }
    // @cpt-end:cpt-cf-oagw-dod-token-cache:p2:inst-token-cache-get-fn-01

    /// Stores `bearer` under `key` for `ttl`. A zero (or already-elapsed)
    /// `ttl` stores nothing, matching the "failed acquisition, or a
    /// too-short-lived token, is never cached" contract
    /// (`cpt-cf-oagw-dod-token-cache`); the caller is responsible for never
    /// calling this after a failed fetch.
    pub fn put(&self, key: &str, bearer: SecretString, ttl: Duration) {
        if ttl.is_zero() {
            return;
        }
        self.cache.put(
            key,
            CachedToken {
                key: key.to_owned(),
                bearer,
            },
            Some(ttl),
        );
    }
}

impl Default for TokenCache {
    fn default() -> Self {
        Self::new(10_000)
    }
}

/// `pingora_memory_cache::MemoryCache` carries no `Debug` impl; this redacts
/// the cache contents entirely rather than exposing cached tokens through a
/// derived field dump.
impl std::fmt::Debug for TokenCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenCache").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::TokenCache;
    use std::time::Duration;
    use toolkit_auth::oauth2::SecretString;

    // @cpt-begin:cpt-cf-oagw-dod-token-cache:p2:inst-token-cache-hit-test-01
    #[test]
    fn a_stored_token_is_returned_on_a_matching_lookup() {
        let cache = TokenCache::new(10);
        cache.put("k1", SecretString::new("tok-1"), Duration::from_mins(1));
        let hit = cache.get("k1").expect("must hit");
        assert_eq!(hit.expose(), "tok-1");
    }
    // @cpt-end:cpt-cf-oagw-dod-token-cache:p2:inst-token-cache-hit-test-01

    #[test]
    fn an_unknown_key_is_a_miss() {
        let cache = TokenCache::new(10);
        assert!(cache.get("nope").is_none());
    }

    #[test]
    fn a_zero_ttl_stores_nothing() {
        let cache = TokenCache::new(10);
        cache.put("k2", SecretString::new("tok-2"), Duration::ZERO);
        assert!(cache.get("k2").is_none());
    }

    // @cpt-begin:cpt-cf-oagw-dod-token-cache:p2:inst-token-cache-ttl-test-01
    #[test]
    fn an_expired_entry_is_a_miss() {
        let cache = TokenCache::new(10);
        cache.put("k3", SecretString::new("tok-3"), Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(50));
        assert!(cache.get("k3").is_none());
    }
    // @cpt-end:cpt-cf-oagw-dod-token-cache:p2:inst-token-cache-ttl-test-01
}
