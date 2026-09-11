//! The Data Plane L1 hot-configuration cache of entry 2.9
//! (`cpt-cf-oagw-flow-observability-and-state-dp-cache-read`,
//! `cpt-cf-oagw-flow-observability-and-state-dp-cache-flush`,
//! `cpt-cf-oagw-algo-observability-and-state-dp-cache-maintenance`).
//!
//! The cache is a **per-instance layer over the resolved configuration the
//! proxy pipeline reads**: one entry per documented key family, holding the
//! record the data plane resolved, together with the *dependency set* — the
//! documented keys of every record the resolution walked through — stamped
//! with the store generation each dependency had when the entry was built
//! (`inst-os-algo-key-4c`).
//!
//! # The documented keys
//!
//! Exactly the three families ADR 0005 records, the same shapes the Control
//! Plane layer derives on write:
//!
//! ```text
//! upstream:{tenant_id}:{alias}               -> the resolved upstream record
//! route:{upstream_id}:{method}:{path_prefix} -> the resolved route record
//! plugin:{plugin_id}                         -> the plugin definition
//! ```
//!
//! A lookup that cannot be expressed as one of the families — the route-match
//! scan over a whole upstream's route list, `list`, `get` by identifier —
//! bypasses the cache instead of minting a new key shape
//! (`inst-os-algo-key-5`), and the effective-configuration merge result is
//! never cached: the cache keys resolved records, not merged documents
//! (`cpt-cf-oagw-principle-no-cache`).
//!
//! # The flush
//!
//! A configuration write flushes every entry whose own key is affected **and**
//! every entry whose dependency set intersects the affected key set, so a
//! descendant tenant whose alias resolved through the written record is
//! invalidated too (`inst-os-algo-key-4c`, `inst-os-cpinv-5`). When the
//! affected key set cannot be derived the whole cache is cleared
//! (`inst-os-algo-inval-8`).
//!
//! # No TTL, no negative caching
//!
//! An entry lives until it is evicted by the LRU or flushed by a write; a
//! lookup the store cannot resolve inserts nothing
//! (`cpt-cf-oagw-dod-observability-and-state-dp-cache`).
// @cpt-state:cpt-cf-oagw-state-observability-and-state-cache-entry:p1

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::domain::dto::Plugin;
use crate::domain::repo::{RouteRecord, UpstreamRecord};
use crate::infra::cp_cache::{CacheKey, ConfigGenerations, L1};

// @cpt-begin:cpt-cf-oagw-state-observability-and-state-cache-entry:p1:inst-os-st-cache-1
// @cpt-begin:cpt-cf-oagw-state-observability-and-state-cache-entry:p1:inst-os-st-cache-2
// @cpt-begin:cpt-cf-oagw-state-observability-and-state-cache-entry:p1:inst-os-st-cache-3
// @cpt-begin:cpt-cf-oagw-state-observability-and-state-cache-entry:p1:inst-os-st-cache-4
// @cpt-begin:cpt-cf-oagw-state-observability-and-state-cache-entry:p1:inst-os-st-cache-5
/// The fixed Data Plane L1 capacity of the graded configuration
/// (`inst-os-deploy-2b`): 1,000 entries, not a configuration key.
pub const DP_L1_CAPACITY: usize = 1_000;
//
// @cpt-end:cpt-cf-oagw-state-observability-and-state-cache-entry:p1:inst-os-st-cache-5
// @cpt-end:cpt-cf-oagw-state-observability-and-state-cache-entry:p1:inst-os-st-cache-4
// @cpt-end:cpt-cf-oagw-state-observability-and-state-cache-entry:p1:inst-os-st-cache-3
// @cpt-end:cpt-cf-oagw-state-observability-and-state-cache-entry:p1:inst-os-st-cache-2
// @cpt-end:cpt-cf-oagw-state-observability-and-state-cache-entry:p1:inst-os-st-cache-1
//

/// The value one Data Plane cache entry holds.
#[derive(Debug, Clone)]
pub enum DpValue {
    /// The resolved upstream record of the `upstream:{tenant_id}:{alias}`
    /// family.
    Upstream(Arc<UpstreamRecord>),
    /// The resolved route record of the `route:{upstream_id}:{method}:{path_prefix}`
    /// family.
    Route(Arc<RouteRecord>),
    /// The plugin definition of the reserved `plugin:{plugin_id}` family.
    Plugin(Arc<Plugin>),
}

/// One entry: the value plus the dependency generations it was built against.
#[derive(Debug, Clone)]
struct DpEntry {
    value: DpValue,
    dependencies: BTreeMap<String, u64>,
}

/// The generations the resolution observed *before* it read the store, which
/// [`DpHotConfig::put`] re-checks so a population that raced a flush cannot
/// insert a pre-write value (`inst-os-algo-lru-4b`).
pub type Observed = BTreeMap<String, u64>;

/// The Data Plane L1 hot-configuration cache
/// (`cpt-cf-oagw-dod-observability-and-state-dp-cache`).
pub struct DpHotConfig {
    l1: L1<DpEntry>,
    generations: ConfigGenerations,
}

impl std::fmt::Debug for DpHotConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DpHotConfig")
            .field("l1", &self.l1)
            .finish()
    }
}

impl DpHotConfig {
    /// The 1,000-entry cache over the shared store generations.
    #[must_use]
    pub fn new(generations: ConfigGenerations) -> Self {
        Self { l1: L1::new(DP_L1_CAPACITY), generations }
    }

    /// The shared store generations this cache stamps its entries with.
    #[must_use]
    pub const fn generations(&self) -> &ConfigGenerations {
        &self.generations
    }

    /// The generation of every key in `keys`, as the resolution observed it
    /// before the store read.
    #[must_use]
    pub fn observe(&self, keys: &[String]) -> Observed {
        keys.iter().map(|key| (key.clone(), self.generations.generation(key))).collect()
    }

    /// The documented key of the upstream family.
    #[must_use]
    pub fn upstream_key(tenant_id: uuid::Uuid, alias: &str) -> String {
        CacheKey::Upstream { owner_tenant_id: tenant_id, alias: alias.to_owned() }.as_string()
    }

    /// The documented key of the route family.
    #[must_use]
    pub fn route_key(upstream_id: uuid::Uuid, method: &str, path_prefix: &str) -> String {
        CacheKey::Route {
            upstream_id,
            method: method.to_owned(),
            path_prefix: path_prefix.to_owned(),
        }
        .as_string()
    }

    /// The documented key of the reserved plugin family.
    #[must_use]
    pub fn plugin_key(plugin_id: uuid::Uuid) -> String {
        CacheKey::Plugin { plugin_id }.as_string()
    }

    /// Insert one entry, stamped with the dependency generations `observed`
    /// holds. An observed generation that has since moved means the value was
    /// read before a write that has already landed: the entry is refused
    /// (`inst-os-algo-lru-4b`).
    pub fn put(&self, key: String, value: DpValue, observed: Observed) {
        let mut dependencies = observed.clone();
        dependencies.entry(key.clone()).or_insert_with(|| self.generations.generation(&key));
        if dependencies
            .iter()
            .any(|(dependency, generation)| self.generations.generation(dependency) != *generation)
        {
            return;
        }
        self.l1.insert(&key, DpEntry { value, dependencies }, 0);
    }

    /// The cached upstream record, when every dependency still carries the
    /// generation the entry was built against.
    #[must_use]
    pub fn get_upstream(&self, tenant_id: uuid::Uuid, alias: &str) -> Option<Arc<UpstreamRecord>> {
        let key = Self::upstream_key(tenant_id, alias);
        self.get(&key).map(|entry| match entry {
            DpValue::Upstream(record) => record,
            _ => unreachable!("the upstream family holds upstream records"),
        })
    }

    /// The cached route record of the route family.
    #[must_use]
    pub fn get_route(
        &self,
        upstream_id: uuid::Uuid,
        method: &str,
        path_prefix: &str,
    ) -> Option<Arc<RouteRecord>> {
        let key = Self::route_key(upstream_id, method, path_prefix);
        self.get(&key).map(|entry| match entry {
            DpValue::Route(record) => record,
            _ => unreachable!("the route family holds route records"),
        })
    }

    /// The cached plugin definition of the reserved plugin family
    /// (`inst-os-algo-key-3`): the family exists and is invalidated, but no
    /// reader in the graded configuration consults it.
    #[must_use]
    pub fn get_plugin(&self, plugin_id: uuid::Uuid) -> Option<Arc<Plugin>> {
        let key = Self::plugin_key(plugin_id);
        self.get(&key).map(|entry| match entry {
            DpValue::Plugin(plugin) => plugin,
            _ => unreachable!("the plugin family holds plugin definitions"),
        })
    }

    /// The live entry at `key`, or `None` — removing it — when any dependency
    /// generation has moved (`inst-os-algo-lru-4b`).
    fn get(&self, key: &str) -> Option<DpValue> {
        let (entry, _) = self.l1.get(key)?;
        if entry
            .dependencies
            .iter()
            .any(|(dependency, generation)| self.generations.generation(dependency) != *generation)
        {
            self.l1.remove(key);
            return None;
        }
        Some(entry.value)
    }

    /// The flush one configuration write triggers
    /// (`inst-os-cpinv-4`, `inst-os-cpinv-5`): every entry whose own key is in
    /// `keys`, plus every entry whose dependency set intersects `keys`.
    pub fn flush(&self, keys: &[CacheKey]) {
        let affected: BTreeSet<String> = keys.iter().map(CacheKey::as_string).collect();
        if affected.is_empty() {
            return;
        }
        self.l1.retain_entries(|key, entry| {
            if affected.contains(key) {
                return false;
            }
            entry.dependencies.keys().all(|dependency| !affected.contains(dependency))
        });
    }

    /// The flush fallback: the whole cache is cleared when the affected key
    /// set of a write cannot be derived (`inst-os-algo-inval-8`).
    pub fn flush_all(&self) {
        self.l1.clear();
    }

    /// How many entries the cache holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.l1.len()
    }

    /// Whether the cache holds no entry.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.l1.is_empty()
    }
}

/// The upstream dependencies of one alias resolution: the documented key of
/// the alias in the calling tenant, plus the documented key of every level the
/// walk found a record at (`inst-os-algo-key-4c`).
#[must_use]
pub fn upstream_dependencies(
    tenant_id: uuid::Uuid,
    alias: &str,
    levels: &[crate::infra::proxy::alias_resolver::AliasLevel],
) -> BTreeSet<String> {
    let mut dependencies = BTreeSet::from([DpHotConfig::upstream_key(tenant_id, alias)]);
    for level in levels {
        if let Some(record) = &level.record {
            dependencies.insert(DpHotConfig::upstream_key(level.tenant_id, &record.upstream.alias));
        }
    }
    dependencies
}

#[cfg(test)]
#[path = "dp_cache_tests.rs"]
mod dp_cache_tests;
