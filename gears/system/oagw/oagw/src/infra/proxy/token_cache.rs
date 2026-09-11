//! The OAuth2 token cache of the client-credentials auth plugins
//! (`cpt-cf-oagw-algo-token-cache`, `cpt-cf-oagw-state-token-cache`).
//!
//! The cache is the plugin's own: a `pingora-memory-cache` bounded by the gear
//! configuration's `token_cache_capacity`, keyed by
//! `subject_tenant_id:subject_id:auth_method_tag:config_hash`, so two upstreams
//! that differ only in `scopes` or in the token endpoint hold different entries
//! (`inst-atc-01`, `inst-atc-02`).
//!
//! Every entry is a [`CachedToken`] wrapper that carries the key it was stored
//! under, and every hit verifies that key: a hash collision is served as a miss
//! and never as another tenant's or another subject's token (`inst-atc-04`,
//! `inst-stc-07`). The token itself is held as a [`SecretString`], so the buffer
//! is zeroed when the entry is reclaimed or evicted (`inst-stc-05`).
//!
//! There is no invalidation hook: a revoked or rotated token stays cached until
//! its TTL expires (`inst-atc-16`), and nothing is persisted, so a restart
//! returns the machine to `Empty`.

use std::collections::BTreeMap;
use std::fmt;
use std::hash::Hasher;
use std::sync::Arc;
use std::time::Duration;

use pingora_memory_cache::{CacheStatus, MemoryCache};
use toolkit_auth::oauth2::SecretString;

/// The seconds of an IdP `expires_in` that are never cached, the safety margin
/// of `min(token_cache_ttl_secs, expires_in - 30s)`.
pub const SAFETY_MARGIN_SECS: u64 = 30;

/// The separator of the cache-key members.
const KEY_SEPARATOR: &str = ":";

/// A cached token and the key it was stored under.
///
/// The token is held through an [`Arc`] because the cache requires clonable
/// entries: the arc shares one buffer instead of copying the secret per
/// lookup, and the buffer is zeroed when its last reference — the cache entry
/// or the injection that consumed it — is dropped.
#[derive(Clone)]
pub struct CachedToken {
    /// The cache key the token was stored under, verified on every hit.
    pub key: String,
    /// The access token.
    pub token: Arc<SecretString>,
}

impl fmt::Debug for CachedToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The token is a credential: it is never rendered.
        formatter
            .debug_struct("CachedToken")
            .field("key", &self.key)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

/// The cache key of one token: subject tenant, subject, client-auth method and
/// a hash of the plugin configuration pairs (`inst-atc-01`).
#[must_use]
pub fn cache_key(
    tenant: &str,
    subject: &str,
    auth_method_tag: &str,
    config_hash: &str,
) -> String {
    format!("{tenant}{KEY_SEPARATOR}{subject}{KEY_SEPARATOR}{auth_method_tag}{KEY_SEPARATOR}{config_hash}")
}

/// The deterministic hash of the plugin configuration pairs, sorted by key
/// (`inst-atc-01`).
///
/// The hash is compared only inside the process that computed it, and a
/// collision is caught by the [`CachedToken::key`] verification, so the
/// standard hasher's fixed seed is enough.
#[must_use]
pub fn config_hash(config: &BTreeMap<String, String>) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (key, value) in config {
        hasher.write(key.as_bytes());
        hasher.write_u8(b'=');
        hasher.write(value.as_bytes());
        hasher.write_u8(b';');
    }
    format!("{:016x}", hasher.finish())
}

/// The TTL an entry is stored under: `min(configured, expires_in - 30s)`
/// (`inst-atc-12`).
///
/// [`None`] when the IdP reported an `expires_in` of at most 30 seconds, or a
/// lifetime the margin leaves no usable part of: such a token is injected for
/// its own request and never cached (`inst-atc-13`, `inst-atc-14`,
/// `inst-stc-06`).
#[must_use]
pub fn entry_ttl(configured: Duration, expires_in: Duration) -> Option<Duration> {
    let margin = Duration::from_secs(SAFETY_MARGIN_SECS);
    if expires_in <= margin {
        return None;
    }
    Some(configured.min(expires_in - margin))
}

/// The token cache of the client-credentials auth plugins.
///
/// The cache carries no invalidation hook of any kind: there is no
/// `invalidate`, no revocation listener and no write path other than `put`,
/// so a revoked or rotated token stays cached until its stored TTL elapses
/// (`cpt-cf-oagw-algo-token-cache` `inst-atc-16`).
pub struct TokenCache {
    cache: MemoryCache<String, CachedToken>,
    configured: Duration,
}

impl fmt::Debug for TokenCache {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TokenCache")
            .field("configured_ttl_secs", &self.configured.as_secs())
            .finish()
    }
}

// @cpt-begin:cpt-cf-oagw-algo-token-cache:p1:inst-atc-16
// The public surface of the cache is `get` and `put` only: there is no
// invalidation hook, no revocation listener and no other write path, so a
// revoked or rotated token stays cached until its stored TTL elapses.
impl TokenCache {
    /// A cache bounded by `capacity`, whose entries live at most `ttl`.
    ///
    /// The cache is built once per chain and shared by every request, so the
    /// bucket `token_cache_capacity` bounds the whole process
    /// (`inst-atc-02`).
    #[must_use]
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            cache: MemoryCache::new(capacity),
            configured: ttl,
        }
    }

    /// The configured entry TTL.
    #[must_use]
    pub const fn configured_ttl(&self) -> Duration {
        self.configured
    }

    /// The verified token stored under `key`, if the cache holds one.
    ///
    /// A miss is an empty slot, an entry whose TTL elapsed, or an entry whose
    /// stored key does not equal the lookup key (`inst-atc-03` to
    /// `inst-atc-05`).
    #[must_use]
    pub fn get(&self, key: &str) -> Option<Arc<SecretString>> {
        // @cpt-begin:cpt-cf-oagw-algo-token-cache:p1:inst-atc-03
        // The lookup observes the stored TTL: a fresh entry is returned, an
        // elapsed one is reclaimed here so the following request performs a
        // fresh fetch (`inst-stc-02`, `inst-stc-03`).
        let (entry, status) = self.cache.get(key);
        // @cpt-begin:cpt-cf-oagw-state-token-cache:p1:inst-stc-02
        // @cpt-begin:cpt-cf-oagw-state-token-cache:p1:inst-stc-03
        // Cached to Expired is the lazy state the underlying cache keeps an
        // elapsed entry in; the lookup that observes it reclaims it, so the
        // following request performs a fresh fetch. Expiry is decided before
        // capacity, so an elapsed entry is observed as expiry and never as an
        // eviction.
        if status == CacheStatus::Expired {
            self.cache.remove(key);
            return None;
        }
        // @cpt-end:cpt-cf-oagw-state-token-cache:p1:inst-stc-03
        // @cpt-end:cpt-cf-oagw-state-token-cache:p1:inst-stc-02
        // @cpt-begin:cpt-cf-oagw-state-token-cache:p1:inst-stc-04
        // Expired to Cached: the miss returns the caller to the fetch path,
        // whose `put` stores the fresh token under the same key.
        // @cpt-end:cpt-cf-oagw-state-token-cache:p1:inst-stc-04
        let entry = entry?;
        // @cpt-end:cpt-cf-oagw-algo-token-cache:p1:inst-atc-03

        // @cpt-begin:cpt-cf-oagw-algo-token-cache:p1:inst-atc-04
        // @cpt-begin:cpt-cf-oagw-algo-token-cache:p1:inst-atc-05
        // @cpt-begin:cpt-cf-oagw-state-token-cache:p1:inst-stc-07
        // A stored key that does not equal the lookup key is a miss, never a
        // hit: the mismatched entry is left in place until its own expiry or
        // eviction (`inst-stc-07`).
        if entry.key != key {
            return None;
        }
        // @cpt-end:cpt-cf-oagw-state-token-cache:p1:inst-stc-07
        // @cpt-end:cpt-cf-oagw-algo-token-cache:p1:inst-atc-05
        // @cpt-end:cpt-cf-oagw-algo-token-cache:p1:inst-atc-04
        Some(entry.token)
    }

    /// Store `token` under `key` for `ttl`.
    ///
    /// Cached to Empty: an entry the `token_cache_capacity` bound evicts is
    /// dropped with its `SecretString`, which zeroes the value on drop.
    /// (`inst-stc-05`)
    ///
    /// Cached to Empty: a lookup that finds a stored key other than the lookup
    /// key is a miss and the entry stays until its own expiry or eviction.
    /// (`inst-stc-07`)
    ///
    /// A TTL of zero stores nothing, which is how a token whose `expires_in`
    /// leaves no usable lifetime stays out of the cache (`inst-atc-14`,
    /// `inst-stc-01`).
    pub fn put(&self, key: &str, token: Arc<SecretString>, ttl: Option<Duration>) {
        // @cpt-begin:cpt-cf-oagw-state-token-cache:p1:inst-stc-01
        // @cpt-begin:cpt-cf-oagw-state-token-cache:p1:inst-stc-06
        // Empty to Cached: a successful fetch with a positive computed TTL
        // stores the wrapper under the key. Empty to Empty: a fetch that
        // failed, that the IdP answered without a usable token, or whose
        // lifetime left nothing of it stores nothing, so the next request for
        // the key retries the IdP.
        if ttl.is_none_or(|ttl| ttl.is_zero()) {
            return;
        }
        // @cpt-end:cpt-cf-oagw-state-token-cache:p1:inst-stc-06
        // @cpt-end:cpt-cf-oagw-state-token-cache:p1:inst-stc-01
        // @cpt-begin:cpt-cf-oagw-state-token-cache:p1:inst-stc-05
        // Cached to Empty: the store is bounded by `token_cache_capacity`, so
        // the insert of one more entry than the bound evicts the least recently
        // used one, whose `SecretString` is dropped and zeroed with it. The
        // cached token never comes back except through a fresh fetch.
        // @cpt-begin:cpt-cf-oagw-algo-token-cache:p1:inst-atc-15
        // The wrapper carries the original key, so every later hit is verified
        // against it.
        self.cache.put(
            key,
            CachedToken {
                key: key.to_owned(),
                token,
            },
            ttl,
        );
        // @cpt-end:cpt-cf-oagw-algo-token-cache:p1:inst-atc-15
        // @cpt-end:cpt-cf-oagw-state-token-cache:p1:inst-stc-05
    }
}
// @cpt-end:cpt-cf-oagw-algo-token-cache:p1:inst-atc-16

#[cfg(test)]
mod tests {
    use super::*;

    fn token(value: &str) -> Arc<SecretString> {
        Arc::new(SecretString::new(value.to_owned()))
    }

    #[test]
    fn the_key_joins_the_four_members_in_order() {
        let key = cache_key("tenant-1", "subject-2", "form", "abc123");
        assert_eq!(key, "tenant-1:subject-2:form:abc123");
    }

    #[test]
    fn the_config_hash_follows_the_sorted_pairs() {
        let mut config = BTreeMap::new();
        config.insert("client_id_ref".to_owned(), "a".to_owned());
        config.insert("scopes".to_owned(), "read write".to_owned());
        let sorted = config_hash(&config);

        let mut reversed = BTreeMap::new();
        reversed.insert("scopes".to_owned(), "read write".to_owned());
        reversed.insert("client_id_ref".to_owned(), "a".to_owned());
        assert_eq!(sorted, config_hash(&reversed), "the order of the input is lost");
        assert_ne!(
            sorted,
            config_hash(&BTreeMap::new()),
            "a different configuration hashes differently"
        );
        assert_eq!(sorted.len(), 16);
    }

    #[test]
    fn the_ttl_is_the_minimum_of_the_configured_and_the_lifetime_margin() {
        let configured = Duration::from_secs(300);
        assert_eq!(entry_ttl(configured, Duration::from_secs(3600)), Some(configured));
        assert_eq!(
            entry_ttl(configured, Duration::from_secs(60)),
            Some(Duration::from_secs(30)),
            "a short-lived token is capped at the margin it leaves"
        );
    }

    #[test]
    fn a_token_without_a_usable_lifetime_is_not_cached() {
        assert_eq!(entry_ttl(Duration::from_secs(300), Duration::from_secs(30)), None);
        assert_eq!(entry_ttl(Duration::from_secs(300), Duration::from_secs(10)), None);
        assert_eq!(entry_ttl(Duration::from_secs(300), Duration::ZERO), None);
    }

    #[test]
    fn a_stored_token_is_served_to_its_own_key_only() {
        let cache = TokenCache::new(8, Duration::from_secs(300));
        cache.put("a:b:form:1", token("token-a"), Some(Duration::from_secs(60)));
        assert_eq!(
            cache.get("a:b:form:1").map(|token| token.expose().to_owned()),
            Some("token-a".to_owned()),
            "the entry is served to its own key"
        );
        assert!(cache.get("a:b:basic:1").is_none(), "another key is a miss");
    }

    #[test]
    fn a_key_mismatch_is_a_miss_and_leaves_the_entry_in_place() {
        // Two keys that a collision could confuse: the wrapper carries the
        // stored key, so the mismatch is decided on the stored value.
        let cache = TokenCache::new(8, Duration::from_secs(300));
        cache.put("k-one", token("token-one"), Some(Duration::from_secs(60)));
        assert!(cache.get("k-two").is_none());
        assert_eq!(
            cache.get("k-one").map(|token| token.expose().to_owned()),
            Some("token-one".to_owned()),
            "the mismatched lookup never displaced the entry"
        );
    }

    #[test]
    fn an_expired_entry_is_reclaimed_on_the_lookup_that_observes_it() {
        let cache = TokenCache::new(8, Duration::from_millis(1));
        cache.put("k", token("token-k"), Some(Duration::from_millis(1)));
        std::thread::sleep(Duration::from_millis(5));
        assert!(cache.get("k").is_none(), "an elapsed TTL is a miss");
        // The reclaimed entry is gone: the next lookup observes an empty slot.
        let (entry, status) = cache.cache.get("k");
        assert_eq!(status, CacheStatus::Miss);
        assert!(entry.is_none());
    }

    #[test]
    fn a_zero_ttl_stores_nothing() {
        let cache = TokenCache::new(8, Duration::from_secs(300));
        cache.put("k", token("token-k"), Some(Duration::ZERO));
        cache.put("k", token("token-k"), None);
        assert!(cache.get("k").is_none());
    }

    #[test]
    fn the_cache_evicts_under_its_capacity_bound() {
        let cache = TokenCache::new(1, Duration::from_secs(300));
        cache.put("one", token("token-one"), Some(Duration::from_secs(60)));
        cache.put("two", token("token-two"), Some(Duration::from_secs(60)));
        // One of the two entries is gone; the cache never grew past its bound.
        let held = [
            cache.get("one").is_some(),
            cache.get("two").is_some(),
        ];
        assert_eq!(held.iter().filter(|held| **held).count(), 1);
    }

    #[test]
    fn the_debug_output_carries_no_token() {
        let cache = TokenCache::new(8, Duration::from_secs(300));
        cache.put("k", token("super-secret-token"), Some(Duration::from_secs(60)));
        let rendered = format!("{cache:?}");
        assert!(!rendered.contains("super-secret-token"), "{rendered}");
        let entry = format!("{:?}", CachedToken {
            key: "k".to_owned(),
            token: token("super-secret-token"),
        });
        assert!(!entry.contains("super-secret-token"), "{entry}");
    }
}
