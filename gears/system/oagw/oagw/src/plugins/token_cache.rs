//! The client-credentials token cache
//! (`cpt-cf-oagw-state-plugin-token-cache-entry`,
//! `cpt-cf-oagw-dod-plugin-token-cache`).
//!
//! An in-process, `token_cache_capacity`-bounded cache keyed by a `u64`
//! hash of the composed cache-key string (`inst-token-acquire-06`). The
//! hash is not collision-free, so every entry also stores a copy of its
//! own full key string; a hit is only honoured once that stored key is
//! verified equal to the lookup key (`inst-token-acquire-09`/`-10`) --
//! the collision-safety boundary this cache exists to enforce. Uses
//! [`tokio::time::Instant`] rather than [`std::time::Instant`] so tests can
//! drive expiry deterministically with a paused, manually-advanced clock
//! instead of sleeping.
//!
//! RF-001/RF-006: [`TokenCache`] is now a process-lifetime singleton
//! (`crate::plugins::runtime::chain_runtime`), reached for real from
//! `crate::proxy::engine` (through `super::oauth2`'s production code, in
//! turn reached from `super::execute`'s chain executor) whenever a
//! request's merged `AuthConfig` names `oauth2_client_cred`/
//! `oauth2_client_cred_basic`, sized from `crate::config::OagwConfig`'s
//! `token_cache_capacity` key instead of a fixed constant this module used
//! to be the only source of truth for.

use std::collections::VecDeque;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use dashmap::DashMap;
use parking_lot::Mutex;
use tokio::time::{Duration, Instant};

use toolkit_auth::oauth2::SecretString;

/// RF-006: the gear-level `token_cache_ttl_secs`/`token_cache_capacity`
/// defaults now live as real, operator-overridable `OagwConfig` fields
/// (`crate::config::DEFAULT_TOKEN_CACHE_TTL_SECS`/
/// `DEFAULT_TOKEN_CACHE_CAPACITY`) -- this module no longer hardcodes its
/// own copies.
/// The safety margin subtracted from the identity provider's `expires_in`
/// before it is compared against the configured ceiling.
pub(crate) const TOKEN_EXPIRY_SAFETY_MARGIN_SECS: u64 = 30;

#[derive(Clone)]
struct CacheEntry {
    key: String,
    token: SecretString,
    expires_at: Instant,
}

/// The token cache itself (`cpt-cf-oagw-dod-plugin-token-cache`).
pub(crate) struct TokenCache {
    entries: DashMap<u64, CacheEntry>,
    capacity: usize,
    // FIFO insertion order for the simple capacity-eviction policy below;
    // a real LRU/S3-FIFO policy is not required by this feature's
    // documented contract, only that capacity pressure evicts *something*
    // without ever serving another tuple's token (guaranteed by the
    // per-hit key verification in `get`, independent of eviction policy).
    order: Mutex<VecDeque<u64>>,
}

impl TokenCache {
    // @cpt-begin:cpt-cf-oagw-state-plugin-token-cache-entry:p2:inst-state-token-cache-07
    // In-process only (a plain `DashMap`, no disk/external store): every
    // entry's `Fresh -> Evicted` transition on process restart or Data
    // Plane re-initialization is realized simply by this cache -- and the
    // process it lives in -- no longer existing; there is no persisted
    // state to reload.
    #[must_use]
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            entries: DashMap::new(),
            capacity: capacity.max(1),
            order: Mutex::new(VecDeque::new()),
        }
    }
    // @cpt-end:cpt-cf-oagw-state-plugin-token-cache-entry:p2:inst-state-token-cache-07

    /// `inst-token-acquire-06`'s fixed-width hash of the composed cache
    /// key string.
    pub(crate) fn key_hash(key: &str) -> u64 {
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        hasher.finish()
    }

    /// Look the key up, verifying the stored entry's own key equals the
    /// lookup key before serving it (`inst-token-acquire-07` through
    /// `-11`). A mismatch, or an entry past its effective time-to-live, is
    /// treated as a miss; an expired entry is opportunistically evicted
    /// (lazy expiry, `cpt-cf-oagw-state-plugin-token-cache-entry`'s
    /// `ExpiringWithinMargin -> Evicted` transition).
    // @cpt-algo:cpt-cf-oagw-algo-plugin-token-acquire:p1
    // @cpt-dod:cpt-cf-oagw-dod-plugin-token-cache:p1
    // @cpt-state:cpt-cf-oagw-state-plugin-token-cache-entry:p2
    // @cpt-begin:cpt-cf-oagw-algo-plugin-token-acquire:p1:inst-token-acquire-07
    // @cpt-begin:cpt-cf-oagw-algo-plugin-token-acquire:p1:inst-token-acquire-08
    // @cpt-begin:cpt-cf-oagw-algo-plugin-token-acquire:p1:inst-token-acquire-09
    // @cpt-begin:cpt-cf-oagw-algo-plugin-token-acquire:p1:inst-token-acquire-10
    // @cpt-begin:cpt-cf-oagw-algo-plugin-token-acquire:p1:inst-token-acquire-11
    pub(crate) fn get(&self, key: &str) -> Option<SecretString> {
        let hash = Self::key_hash(key);
        let now = Instant::now();
        let mut expired = false;
        let hit = self.entries.get(&hash).and_then(|entry| {
            if entry.key != key {
                // Hash collision: never return another key's token.
                None
            } else if entry.expires_at <= now {
                // @cpt-begin:cpt-cf-oagw-state-plugin-token-cache-entry:p2:inst-state-token-cache-04
                // The entry has crossed its effective-time-to-live boundary
                // (`Fresh -> ExpiringWithinMargin`); this cache has no
                // separate stored state for that boundary, so the very next
                // observation of it (right here) immediately continues into
                // the `ExpiringWithinMargin -> Evicted` lazy-expiry
                // transition below.
                expired = true;
                // @cpt-end:cpt-cf-oagw-state-plugin-token-cache-entry:p2:inst-state-token-cache-04
                None
            } else {
                // @cpt-begin:cpt-cf-oagw-state-plugin-token-cache-entry:p2:inst-state-token-cache-03
                Some(entry.token.clone())
                // @cpt-end:cpt-cf-oagw-state-plugin-token-cache-entry:p2:inst-state-token-cache-03
            }
        });
        if expired {
            // @cpt-begin:cpt-cf-oagw-state-plugin-token-cache-entry:p2:inst-state-token-cache-05
            // @cpt-begin:cpt-cf-oagw-state-plugin-token-cache-entry:p2:inst-state-token-cache-08
            // Lazy expiry: the next lookup for this key observes the
            // boundary and reclaims the entry's storage, dropping (and so
            // zeroing, via `SecretString`'s `ZeroizeOnDrop`) its token
            // bytes and returning the key to `Absent`.
            self.entries.remove(&hash);
            // @cpt-end:cpt-cf-oagw-state-plugin-token-cache-entry:p2:inst-state-token-cache-08
            // @cpt-end:cpt-cf-oagw-state-plugin-token-cache-entry:p2:inst-state-token-cache-05
        }
        hit
    }
    // @cpt-end:cpt-cf-oagw-algo-plugin-token-acquire:p1:inst-token-acquire-11
    // @cpt-end:cpt-cf-oagw-algo-plugin-token-acquire:p1:inst-token-acquire-10
    // @cpt-end:cpt-cf-oagw-algo-plugin-token-acquire:p1:inst-token-acquire-09
    // @cpt-end:cpt-cf-oagw-algo-plugin-token-acquire:p1:inst-token-acquire-08
    // @cpt-end:cpt-cf-oagw-algo-plugin-token-acquire:p1:inst-token-acquire-07

    /// Store a token under `key` for `ttl` (`inst-token-acquire-19`),
    /// evicting the oldest tracked entry when `capacity` would otherwise
    /// be exceeded (`cpt-cf-oagw-state-plugin-token-cache-entry`'s
    /// `Fresh -> Evicted` capacity-pressure transition).
    // @cpt-begin:cpt-cf-oagw-algo-plugin-token-acquire:p1:inst-token-acquire-19
    // @cpt-begin:cpt-cf-oagw-state-plugin-token-cache-entry:p2:inst-state-token-cache-01
    pub(crate) fn put(&self, key: String, token: SecretString, ttl: Duration) {
        let hash = Self::key_hash(&key);
        let expires_at = Instant::now() + ttl;
        let is_new_key = !self.entries.contains_key(&hash);
        self.entries.insert(
            hash,
            CacheEntry {
                key,
                token,
                expires_at,
            },
        );
        let mut order = self.order.lock();
        if is_new_key {
            order.push_back(hash);
        }
        // @cpt-end:cpt-cf-oagw-state-plugin-token-cache-entry:p2:inst-state-token-cache-01
        // @cpt-begin:cpt-cf-oagw-state-plugin-token-cache-entry:p2:inst-state-token-cache-06
        // Capacity-pressure eviction: reclaims the oldest tracked entry
        // before its own time-to-live elapses, zeroing its token bytes on
        // drop the same way lazy expiry does.
        while self.entries.len() > self.capacity {
            let Some(oldest) = order.pop_front() else {
                break;
            };
            self.entries.remove(&oldest);
        }
        // @cpt-end:cpt-cf-oagw-state-plugin-token-cache-entry:p2:inst-state-token-cache-06
    }
    // @cpt-end:cpt-cf-oagw-algo-plugin-token-acquire:p1:inst-token-acquire-19

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Test-only: insert an entry directly under an arbitrary hash slot
    /// with a mismatched key, to exercise the collision-safety
    /// verification in [`Self::get`] without needing a real `u64` hash
    /// collision.
    #[cfg(test)]
    pub(crate) fn insert_raw(&self, hash: u64, key: String, token: SecretString, ttl: Duration) {
        self.entries.insert(
            hash,
            CacheEntry {
                key,
                token,
                expires_at: Instant::now() + ttl,
            },
        );
    }
}

/// `min(ttl_ceiling_secs, expires_in_secs - 30s safety margin)`
/// (`inst-token-acquire-16`); `None` when `expires_in_secs` is at or below
/// the margin (`inst-token-acquire-17`/`-18`: use for this request only,
/// write no cache entry).
// @cpt-begin:cpt-cf-oagw-algo-plugin-token-acquire:p1:inst-token-acquire-16
// @cpt-begin:cpt-cf-oagw-algo-plugin-token-acquire:p1:inst-token-acquire-17
// @cpt-begin:cpt-cf-oagw-algo-plugin-token-acquire:p1:inst-token-acquire-18
pub(crate) fn effective_ttl_secs(ttl_ceiling_secs: u64, expires_in_secs: u64) -> Option<u64> {
    if expires_in_secs <= TOKEN_EXPIRY_SAFETY_MARGIN_SECS {
        return None;
    }
    let margin_adjusted = expires_in_secs - TOKEN_EXPIRY_SAFETY_MARGIN_SECS;
    Some(ttl_ceiling_secs.min(margin_adjusted))
}
// @cpt-end:cpt-cf-oagw-algo-plugin-token-acquire:p1:inst-token-acquire-18
// @cpt-end:cpt-cf-oagw-algo-plugin-token-acquire:p1:inst-token-acquire-17
// @cpt-end:cpt-cf-oagw-algo-plugin-token-acquire:p1:inst-token-acquire-16

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn effective_ttl_is_the_smaller_of_ceiling_and_margin_adjusted_expiry() {
        assert_eq!(effective_ttl_secs(300, 3600), Some(300));
        assert_eq!(effective_ttl_secs(300, 100), Some(70));
    }

    #[test]
    fn expires_in_at_or_below_margin_is_not_cached() {
        assert_eq!(effective_ttl_secs(300, 30), None);
        assert_eq!(effective_ttl_secs(300, 10), None);
        assert_eq!(effective_ttl_secs(300, 0), None);
    }

    #[tokio::test]
    async fn put_then_get_returns_the_cached_token_without_recomputation() {
        let cache = TokenCache::new(10);
        cache.put(
            "tenant:subject:form:123".to_owned(),
            SecretString::new("tok-a"),
            Duration::from_secs(60),
        );
        let hit = cache.get("tenant:subject:form:123").unwrap();
        assert_eq!(hit.expose(), "tok-a");
    }

    #[test]
    fn miss_on_an_absent_key() {
        let cache = TokenCache::new(10);
        assert!(cache.get("nope").is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn entry_stops_being_served_once_its_ttl_elapses() {
        let cache = TokenCache::new(10);
        cache.put(
            "k".to_owned(),
            SecretString::new("tok"),
            Duration::from_secs(60),
        );
        assert!(cache.get("k").is_some());
        tokio::time::advance(Duration::from_secs(61)).await;
        assert!(cache.get("k").is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn expired_entry_is_lazily_evicted_and_the_next_request_can_repopulate_it() {
        let cache = TokenCache::new(10);
        cache.put(
            "k".to_owned(),
            SecretString::new("old"),
            Duration::from_secs(10),
        );
        tokio::time::advance(Duration::from_secs(11)).await;
        assert!(cache.get("k").is_none());
        assert_eq!(cache.len(), 0, "the expired entry must have been evicted");
        cache.put(
            "k".to_owned(),
            SecretString::new("fresh"),
            Duration::from_secs(60),
        );
        assert_eq!(cache.get("k").unwrap().expose(), "fresh");
    }

    #[test]
    fn a_stored_key_mismatch_on_the_same_hash_slot_is_treated_as_a_miss() {
        let cache = TokenCache::new(10);
        let hash = TokenCache::key_hash("lookup-key");
        cache.insert_raw(
            hash,
            "a-different-key".to_owned(),
            SecretString::new("someone-elses-token"),
            Duration::from_secs(60),
        );
        assert!(
            cache.get("lookup-key").is_none(),
            "a colliding entry for a different key must never be served"
        );
    }

    #[tokio::test]
    async fn capacity_pressure_evicts_without_ever_serving_another_tuples_token() {
        let cache = TokenCache::new(2);
        cache.put(
            "a".to_owned(),
            SecretString::new("tok-a"),
            Duration::from_secs(60),
        );
        cache.put(
            "b".to_owned(),
            SecretString::new("tok-b"),
            Duration::from_secs(60),
        );
        cache.put(
            "c".to_owned(),
            SecretString::new("tok-c"),
            Duration::from_secs(60),
        );

        assert!(cache.len() <= 2);
        // Whichever entries remain must serve only their own token.
        if let Some(token) = cache.get("a") {
            assert_eq!(token.expose(), "tok-a");
        }
        if let Some(token) = cache.get("b") {
            assert_eq!(token.expose(), "tok-b");
        }
        let token_c = cache
            .get("c")
            .expect("the most recent entry must survive eviction");
        assert_eq!(token_c.expose(), "tok-c");
    }
}
