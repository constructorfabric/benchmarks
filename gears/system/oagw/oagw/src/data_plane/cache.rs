//! The Data Plane L1 configuration cache.
//!
//! Realizes `cpt-cf-oagw-algo-dp-cache`: a per-instance LRU of 1000 entries
//! with no TTL, populated lazily on read, invalidated explicitly and in process
//! by the flush the configuration write path notifies, and holding exactly the
//! resolved upstream configurations and their route candidate sets that ADR
//! 0006's DP State scopes to the Data Plane. ADR 0005's two key shapes are
//! narrowed here as the FEATURE §1.5 table records: `upstream:{tenant_id}:{alias}`
//! for the entry itself and `route:{upstream_id}:{method}:{path_prefix}` for the
//! route keys the entry's candidate set was read under, which the prefix flush
//! drops with it.
//!
//! The cache never holds a response body, credential material, a cached access
//! token, or rate-limit state: `cpt-cf-oagw-principle-no-cache` places the
//! response on the caller and the upstream, and the credential material belongs
//! to the chain. It runs no periodic sync, no TTL expiry, and no background
//! refresh — the explicit flush is the only mechanism.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use parking_lot::Mutex;
use uuid::Uuid;

use crate::domain::proxy::ResolvedUpstream;

/// The entry count ADR 0006 fixes for the Data Plane L1 cache.
pub const DP_CACHE_CAPACITY: usize = 1000;

// @cpt-dod:cpt-cf-oagw-dod-dp-cache:p1

/// One cached resolution: the resolved configuration and the route keys its
/// candidate set was read under.
struct Entry {
    /// The upstream the entry resolves: the identity the upstream-scoped flush
    /// matches on, since the key itself carries the alias.
    upstream_id: Uuid,
    value: Arc<ResolvedUpstream>,
    /// The `route:{upstream_id}:{method}:{path_prefix}` keys the entry covers,
    /// dropped together with it by the prefix flush.
    route_keys: Vec<String>,
}

/// The Data Plane L1 configuration cache.
///
/// Cloned handles share one cache, which is what makes the flush of one write
/// visible to every later read in the process.
#[derive(Clone, Default)]
pub struct DpCache {
    inner: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    entries: HashMap<String, Entry>,
    /// Least-recently-used order, least recent first.
    order: VecDeque<String>,
}

impl DpCache {
    /// Creates an empty cache.
    #[must_use]
    pub fn new() -> Self {
        // @cpt-begin:cpt-cf-oagw-algo-dp-cache:p1:inst-cache-nosync
        // The cache runs no periodic sync, no TTL expiry, and no background
        // refresh: the explicit flush the configuration write path notifies is
        // the only mechanism that removes an entry, which is what
        // DECOMPOSITION §1.3(10) records and what keeps the invalidation in the
        // same process as the write that required it.
        // @cpt-end:cpt-cf-oagw-algo-dp-cache:p1:inst-cache-nosync
        Self::default()
    }

    /// The `upstream:{tenant_id}:{alias}` key shape of ADR 0005.
    #[must_use]
    pub fn upstream_key(tenant_id: Uuid, alias: &str) -> String {
        format!("upstream:{tenant_id}:{alias}")
    }

    /// The `route:{upstream_id}:{method}:{path_prefix}` key shape of ADR 0005.
    #[must_use]
    pub fn route_key(upstream_id: Uuid, method: &str, path_prefix: &str) -> String {
        format!("route:{upstream_id}:{method}:{path_prefix}")
    }

    /// Looks a resolved configuration up, marking it most recently used.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<Arc<ResolvedUpstream>> {
        // @cpt-begin:cpt-cf-oagw-algo-dp-cache:p1:inst-cache-lookup
        // The read path of `cpt-cf-oagw-algo-resolve-consume` looks the key up
        // here and the entry comes back when it is present, with the hit moved
        // to the most recently used end of the order.
        let mut state = self.inner.lock();
        let value = state.entries.get(key)?;
        let hit = Arc::clone(&value.value);
        state.order.retain(|candidate| candidate != key);
        state.order.push_back(String::from(key));
        // @cpt-begin:cpt-cf-oagw-algo-dp-cache:p1:inst-cache-return
        // RETURN the hit, the inserted entry, or the invalidated key set: this
        // branch is the hit, the insert answers the miss, and the flush
        // answers the invalidation.
        Some(hit)
        // @cpt-end:cpt-cf-oagw-algo-dp-cache:p1:inst-cache-return
        // @cpt-end:cpt-cf-oagw-algo-dp-cache:p1:inst-cache-lookup
    }

    /// Inserts a resolution under the upstream key, recording the route keys it
    /// covers, and evicts the least-recently-used entry at the capacity.
    pub fn insert(&self, key: String, value: Arc<ResolvedUpstream>, route_keys: Vec<String>) {
        // @cpt-begin:cpt-cf-oagw-algo-dp-cache:p1:inst-cache-miss-else
        // The ELSE of the lookup: the resolution runs and its result arrives
        // here, so the next read of the key is a hit.
        // @cpt-end:cpt-cf-oagw-algo-dp-cache:p1:inst-cache-miss-else
        // @cpt-begin:cpt-cf-oagw-algo-dp-cache:p1:inst-cache-insert
        let mut state = self.inner.lock();
        state.order.retain(|candidate| candidate != &key);
        state.order.push_back(key.clone());
        state.entries.insert(
            key,
            Entry {
                upstream_id: value.upstream_id,
                value,
                route_keys,
            },
        );
        // The least-recently-used entry leaves when the 1000-entry ceiling of
        // ADR 0006 is reached, and no entry beyond it is ever held.
        while state.entries.len() > DP_CACHE_CAPACITY {
            let Some(evicted) = state.order.pop_front() else {
                break;
            };
            state.entries.remove(&evicted);
        }
        // @cpt-end:cpt-cf-oagw-algo-dp-cache:p1:inst-cache-insert
    }

    /// Flushes the entries of one tenant: the tenant's upstream keys and the
    /// route keys those entries cover, and nothing else.
    ///
    /// This is the flush `cpt-cf-oagw-algo-dp-cache` step 3 executes when the
    /// configuration write path notifies it, so one write never discards an
    /// unrelated tenant's entries.
    pub fn flush_tenant(&self, tenant_id: Uuid) {
        let prefix = format!("upstream:{tenant_id}:");
        self.flush_by(|key, _entry| key.starts_with(&prefix));
    }

    /// Flushes one upstream's entry and its route keys, leaving the rest of the
    /// tenant's entries in place.
    pub fn flush_upstream(&self, tenant_id: Uuid, upstream_id: Uuid) {
        let prefix = format!("upstream:{tenant_id}:");
        self.flush_by(|key, entry| {
            key.starts_with(&prefix) && entry.upstream_id == upstream_id
        });
    }

    /// The number of entries the cache holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().entries.len()
    }

    /// Whether the cache holds no entry.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.lock().entries.is_empty()
    }

    /// Drops every entry the predicate names, together with the route keys the
    /// dropped entries recorded.
    fn flush_by(&self, doomed_by: impl Fn(&str, &Entry) -> bool) {
        let mut state = self.inner.lock();
        let doomed: Vec<String> = state
            .entries
            .iter()
            .filter(|(key, entry)| doomed_by(key, entry))
            .map(|(key, _)| key.clone())
            .collect();
        if doomed.is_empty() {
            return;
        }
        let mut route_keys: Vec<String> = Vec::new();
        for key in &doomed {
            if let Some(entry) = state.entries.remove(key) {
                route_keys.extend(entry.route_keys);
            }
            state.order.retain(|candidate| candidate != key);
        }
        for key in route_keys {
            state.entries.remove(&key);
            state.order.retain(|candidate| candidate != &key);
        }
    }
}

// @cpt-begin:cpt-cf-oagw-algo-dp-cache:p1:inst-cache-invalidate-if
// The write path of `cpt-cf-oagw-feature-control-plane-config` notifies this
// feature's flush routine through the seam the generation advance rides: a
// successful write of any kind reaches here in the same process and before the
// write's response is produced.
impl crate::control_plane::cache::DataPlaneFlush for DpCache {
    fn configuration_written(&self, tenant_id: Uuid) {
        // @cpt-begin:cpt-cf-oagw-algo-dp-cache:p1:inst-cache-invalidate
        // The flush this feature owns: the entries the write affects leave
        // now, so a read that follows the write never resolves against a chain
        // the write superseded.
        // @cpt-end:cpt-cf-oagw-algo-dp-cache:p1:inst-cache-invalidate
        // @cpt-begin:cpt-cf-oagw-algo-dp-cache:p1:inst-cache-flush-prefix
        // The flush is by key prefix — this tenant's upstream keys and the
        // route keys those entries cover — so one write never discards an
        // unrelated tenant's entries.
        self.flush_tenant(tenant_id);
        // @cpt-end:cpt-cf-oagw-algo-dp-cache:p1:inst-cache-flush-prefix
    }
}
// @cpt-end:cpt-cf-oagw-algo-dp-cache:p1:inst-cache-invalidate-if

// @cpt-begin:cpt-cf-oagw-algo-dp-cache:p1:inst-cache-invalidate-else
// The ELSE of the notification: a write that failed against the store returns
// before it reaches the seam, and reaches this routine never.
// @cpt-end:cpt-cf-oagw-algo-dp-cache:p1:inst-cache-invalidate-else

// @cpt-begin:cpt-cf-oagw-algo-dp-cache:p1:inst-cache-invalidate-none
// So a failed write leaves the cache untouched: the database the write was
// tried against is unchanged, and the entries it would have dropped still hold
// the configuration every read is subject to.
// @cpt-end:cpt-cf-oagw-algo-dp-cache:p1:inst-cache-invalidate-none
