//! OAuth2 access-token cache (ADR 0008).

use std::time::Duration;

use pingora_memory_cache::MemoryCache;
use toolkit_auth::oauth2::SecretString;

/// A cached access token paired with the cache key it was stored under.
///
/// ADR 0008 requires the key to be re-verified on a hit so a hit computed
/// from a *different* credential set can never be served.
#[derive(Clone)]
pub struct CachedToken {
    /// The cache key the token was stored under.
    pub key: String,
    /// The bearer value. `SecretString` redacts it in `Debug`/`Display`.
    pub token: SecretString,
}

/// In-process token cache keyed by
/// `subject_tenant_id:subject_id:auth_method_tag:hash(config)`.
pub struct TokenCache {
    inner: MemoryCache<String, CachedToken>,
}

impl std::fmt::Debug for TokenCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenCache").finish_non_exhaustive()
    }
}

impl TokenCache {
    /// A cache holding at most `capacity` entries.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: MemoryCache::new(capacity.max(1)),
        }
    }

    /// Look a token up, verifying the stored key still matches.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<CachedToken> {
        let (hit, _status) = self.inner.get(key);
        hit.filter(|cached| cached.key == key)
    }

    /// Insert a token with a TTL.
    pub fn put(&self, key: String, token: String, ttl: Duration) {
        if token.is_empty() || ttl.is_zero() {
            return;
        }
        let stored_key = key.clone();
        self.inner.put(
            &key,
            CachedToken {
                key: stored_key,
                token: SecretString::new(token),
            },
            Some(ttl),
        );
    }

    /// Drop an entry (used when a cached token is rejected upstream).
    pub fn remove(&self, key: &str) {
        self.inner.remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_token_and_rejects_a_mismatched_key() {
        let cache = TokenCache::new(16);
        cache.put(
            "a:1".to_owned(),
            "tok-1".to_owned(),
            Duration::from_secs(60),
        );
        let hit = cache.get("a:1").expect("hit");
        assert_eq!(hit.key, "a:1");
        assert_eq!(hit.token.expose(), "tok-1");

        // A cached entry can only be served for the key it was stored under.
        assert!(cache.get("a:2").is_none());
    }

    #[test]
    fn empty_tokens_and_zero_ttls_are_not_stored() {
        let cache = TokenCache::new(4);
        cache.put("k".to_owned(), String::new(), Duration::from_secs(60));
        cache.put("k2".to_owned(), "v".to_owned(), Duration::from_secs(0));
        assert!(cache.get("k").is_none());
        assert!(cache.get("k2").is_none());
    }

    #[test]
    fn expired_entries_are_dropped() {
        let cache = TokenCache::new(4);
        cache.put("k".to_owned(), "v".to_owned(), Duration::from_millis(20));
        std::thread::sleep(Duration::from_millis(60));
        assert!(cache.get("k").is_none());
    }

    #[test]
    fn tokens_never_leak_through_debug() {
        let cache = TokenCache::new(4);
        cache.put(
            "k".to_owned(),
            "super-secret-value".to_owned(),
            Duration::from_secs(60),
        );
        let rendered = format!("{cache:?}");
        assert!(!rendered.contains("super-secret-value"));
    }
}
