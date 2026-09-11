//! The Control Plane L1 configuration cache of entry 2.9
//! (`cpt-cf-oagw-flow-observability-and-state-cp-cache-read`,
//! `cpt-cf-oagw-flow-observability-and-state-cp-cache-invalidation`,
//! `cpt-cf-oagw-algo-observability-and-state-l1-cache-maintenance`).
//!
//! The cache is a **decorator over the domain repository trait**: the
//! infrastructure layer supplies a caching `UpstreamRepository`, so no domain
//! service and no resolver changes shape to gain a cache. Only the read the
//! three key families can express is cached — `get_by_alias`, the lookup one
//! alias-resolution step performs — and every other operation delegates, so a
//! lookup that cannot be expressed in one of the families bypasses the cache
//! instead of minting a new key shape (`inst-os-algo-key-5`).
//!
//! # The four read outcomes
//!
//! A read returns exactly one of
//! (`inst-os-cpread-10`, `inst-os-algo-lru-8`):
//!
//! * the cached value (`Cached`);
//! * the resolved value, populated on the way out (`Resolved`);
//! * the distinct not-found outcome, with nothing inserted
//!   (`inst-os-cpread-9`);
//! * the distinct store-error domain error, with nothing inserted
//!   (`inst-os-cpread-9c`).
//!
//! # The generation guard
//!
//! Every entry is stamped with the store generation observed when the value
//! was read, and an insert is accepted only when that generation still equals
//! the store's current generation for the key
//! (`inst-os-algo-lru-4b`, `inst-os-algo-inval-3b`), so a population that
//! raced a flush cannot insert a pre-write value.
// @cpt-algo:cpt-cf-oagw-algo-observability-and-state-audit-record:p1
// @cpt-algo:cpt-cf-oagw-algo-observability-and-state-deployment-mode:p1
// @cpt-algo:cpt-cf-oagw-algo-observability-and-state-metric-label-normalization:p1

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::repo::{UpstreamRecord, UpstreamRepository};

// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-audit-record:p1:inst-os-algo-audit-1
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-audit-record:p1:inst-os-algo-audit-2
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-audit-record:p1:inst-os-algo-audit-2b
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-audit-record:p1:inst-os-algo-audit-2c
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-audit-record:p1:inst-os-algo-audit-2d
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-audit-record:p1:inst-os-algo-audit-3
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-audit-record:p1:inst-os-algo-audit-4
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-audit-record:p1:inst-os-algo-audit-5
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-audit-record:p1:inst-os-algo-audit-5b
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-audit-record:p1:inst-os-algo-audit-6
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-audit-record:p1:inst-os-algo-audit-7
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-deployment-mode:p1:inst-os-algo-deploy-1
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-deployment-mode:p1:inst-os-algo-deploy-1b
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-deployment-mode:p1:inst-os-algo-deploy-2
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-deployment-mode:p1:inst-os-algo-deploy-3
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-deployment-mode:p1:inst-os-algo-deploy-4
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-deployment-mode:p1:inst-os-algo-deploy-5
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-deployment-mode:p1:inst-os-algo-deploy-6
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-deployment-mode:p1:inst-os-algo-deploy-7
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-metric-label-normalization:p1:inst-os-algo-label-1
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-metric-label-normalization:p1:inst-os-algo-label-10
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-metric-label-normalization:p1:inst-os-algo-label-2
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-metric-label-normalization:p1:inst-os-algo-label-3
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-metric-label-normalization:p1:inst-os-algo-label-4
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-metric-label-normalization:p1:inst-os-algo-label-5
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-metric-label-normalization:p1:inst-os-algo-label-6
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-metric-label-normalization:p1:inst-os-algo-label-7
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-metric-label-normalization:p1:inst-os-algo-label-7b
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-metric-label-normalization:p1:inst-os-algo-label-8
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-metric-label-normalization:p1:inst-os-algo-label-9
/// The fixed Control Plane L1 capacity of the graded configuration
/// (`inst-os-deploy-2b`): 10,000 entries, not a configuration key.
pub const CP_L1_CAPACITY: usize = 10_000;
//
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-audit-record:p1:inst-os-algo-audit-7
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-audit-record:p1:inst-os-algo-audit-6
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-audit-record:p1:inst-os-algo-audit-5b
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-audit-record:p1:inst-os-algo-audit-5
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-audit-record:p1:inst-os-algo-audit-4
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-audit-record:p1:inst-os-algo-audit-3
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-audit-record:p1:inst-os-algo-audit-2d
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-audit-record:p1:inst-os-algo-audit-2c
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-audit-record:p1:inst-os-algo-audit-2b
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-audit-record:p1:inst-os-algo-audit-2
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-audit-record:p1:inst-os-algo-audit-1
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-deployment-mode:p1:inst-os-algo-deploy-7
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-deployment-mode:p1:inst-os-algo-deploy-6
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-deployment-mode:p1:inst-os-algo-deploy-5
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-deployment-mode:p1:inst-os-algo-deploy-4
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-deployment-mode:p1:inst-os-algo-deploy-3
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-deployment-mode:p1:inst-os-algo-deploy-2
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-deployment-mode:p1:inst-os-algo-deploy-1b
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-deployment-mode:p1:inst-os-algo-deploy-1
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-metric-label-normalization:p1:inst-os-algo-label-9
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-metric-label-normalization:p1:inst-os-algo-label-8
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-metric-label-normalization:p1:inst-os-algo-label-7b
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-metric-label-normalization:p1:inst-os-algo-label-7
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-metric-label-normalization:p1:inst-os-algo-label-6
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-metric-label-normalization:p1:inst-os-algo-label-5
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-metric-label-normalization:p1:inst-os-algo-label-4
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-metric-label-normalization:p1:inst-os-algo-label-3
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-metric-label-normalization:p1:inst-os-algo-label-2
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-metric-label-normalization:p1:inst-os-algo-label-10
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-metric-label-normalization:p1:inst-os-algo-label-1
//

/// The three `CacheKey` families ADR 0005 records
/// (`cpt-cf-oagw-dod-observability-and-state-cache-keys`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CacheKey {
    /// `upstream:{owner_tenant_id}:{alias}`
    Upstream { owner_tenant_id: Uuid, alias: String },
    /// `route:{upstream_id}:{method}:{path_prefix}`
    Route { upstream_id: Uuid, method: String, path_prefix: String },
    /// `plugin:{plugin_id}` — the reserved family with no reader in the
    /// graded configuration (`inst-os-algo-key-3`).
    Plugin { plugin_id: Uuid },
}

impl CacheKey {
    /// The key's canonical string form.
    #[must_use]
    pub fn as_string(&self) -> String {
        match self {
            Self::Upstream { owner_tenant_id, alias } => {
                format!("upstream:{owner_tenant_id}:{alias}")
            }
            Self::Route { upstream_id, method, path_prefix } => {
                format!("route:{upstream_id}:{method}:{path_prefix}")
            }
            Self::Plugin { plugin_id } => format!("plugin:{plugin_id}"),
        }
    }

    /// Parse a canonical string form back into a key, or `None` when the
    /// string is not one of the three families.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        let (family, rest) = value.split_once(':')?;
        match family {
            "upstream" => {
                let (tenant, alias) = rest.split_once(':')?;
                let owner_tenant_id = Uuid::parse_str(tenant).ok()?;
                Some(Self::Upstream { owner_tenant_id, alias: alias.to_owned() })
            }
            "route" => {
                let (upstream, rest) = rest.split_once(':')?;
                let (method, path_prefix) = rest.split_once(':')?;
                let upstream_id = Uuid::parse_str(upstream).ok()?;
                Some(Self::Route {
                    upstream_id,
                    method: method.to_owned(),
                    path_prefix: path_prefix.to_owned(),
                })
            }
            "plugin" => {
                let plugin_id = Uuid::parse_str(rest).ok()?;
                Some(Self::Plugin { plugin_id })
            }
            _ => None,
        }
    }
}

// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-cache-key-derivation:p1:inst-os-algo-key-1
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-cache-key-derivation:p1:inst-os-algo-key-3
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-cache-key-derivation:p1:inst-os-algo-key-4
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-cache-key-derivation:p1:inst-os-algo-key-4b
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-cache-key-derivation:p1:inst-os-algo-key-4c
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-cache-key-derivation:p1:inst-os-algo-key-5
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-cache-key-derivation:p1:inst-os-algo-key-5b
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-cache-key-derivation:p1:inst-os-algo-key-5c
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-cache-key-derivation:p1:inst-os-algo-key-5d
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-cache-key-derivation:p1:inst-os-algo-key-6
// @cpt-begin:cpt-cf-oagw-algo-observability-and-state-cache-key-derivation:p1:inst-os-algo-key-2
/// The affected route keys of one written route record
/// (`cpt-cf-oagw-algo-observability-and-state-cache-key-derivation`): one key
/// per method of the `http` match block, with the normalized match pattern as
/// the path component. A route whose match block is `grpc` — a configuration
/// surface only in the graded configuration — derives no key, because no
/// reader can ever express one.
#[must_use]
pub fn route_keys_of(route: &crate::domain::dto::Route) -> Vec<CacheKey> {
    let Some(http) = &route.match_.http else { return Vec::new() };
    http.methods
        .iter()
        .map(|method| CacheKey::Route {
            upstream_id: route.upstream_id,
            method: method.as_str().to_owned(),
            path_prefix: http.path.clone(),
        })
        .collect()
}
//
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-cache-key-derivation:p1:inst-os-algo-key-6
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-cache-key-derivation:p1:inst-os-algo-key-5d
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-cache-key-derivation:p1:inst-os-algo-key-5c
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-cache-key-derivation:p1:inst-os-algo-key-5b
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-cache-key-derivation:p1:inst-os-algo-key-5
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-cache-key-derivation:p1:inst-os-algo-key-4c
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-cache-key-derivation:p1:inst-os-algo-key-4b
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-cache-key-derivation:p1:inst-os-algo-key-4
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-cache-key-derivation:p1:inst-os-algo-key-3
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-cache-key-derivation:p1:inst-os-algo-key-1
//
// @cpt-end:cpt-cf-oagw-algo-observability-and-state-cache-key-derivation:p1:inst-os-algo-key-2

impl std::fmt::Display for CacheKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.as_string())
    }
}

/// The shared, per-key generation counter the configuration store carries
/// (`inst-os-algo-inval-3b`).
///
/// Every accepted write bumps the generation of every key it touches —
/// including a write made straight through the raw store by a test or by a
/// provisioning path — so a cached entry stamped with the previous generation
/// can never be inserted.
#[derive(Debug, Default, Clone)]
pub struct ConfigGenerations {
    generations: Arc<Mutex<HashMap<String, u64>>>,
}

impl ConfigGenerations {
    /// The current generation of `key`; `0` for a key nothing has written.
    #[must_use]
    pub fn generation(&self, key: &str) -> u64 {
        self.generations.lock().get(key).copied().unwrap_or(0)
    }

    /// Bump the generation of `key`.
    pub fn bump(&self, key: &str) {
        let mut generations = self.generations.lock();
        let entry = generations.entry(key.to_owned()).or_insert(0);
        *entry = entry.wrapping_add(1);
    }
}

/// One L1 cache entry: the value, its generation and its last-used stamp.
struct Entry<V> {
    value: V,
    generation: u64,
    last_used: u64,
}

/// The per-instance LRU of one L1 layer
/// (`inst-os-algo-lru-1`, `inst-os-algo-lru-5`).
///
/// The guard is held for the lookup plus the LRU order update and never
/// across I/O (`inst-os-algo-lru-1b`); the store read happens outside it. A
/// hit moves the entry to the most recently used position and a miss leaves
/// the cache untouched until the backing read has returned.
pub struct L1<V: Clone> {
    capacity: usize,
    entries: Mutex<(HashMap<String, Entry<V>>, u64)>,
}

impl<V: Clone> std::fmt::Debug for L1<V> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("L1")
            .field("capacity", &self.capacity)
            .field("entries", &self.entries.lock().0.len())
            .finish()
    }
}

impl<V: Clone> L1<V> {
    pub(crate) fn new(capacity: usize) -> Self {
        Self { capacity, entries: Mutex::new((HashMap::new(), 0)) }
    }

    /// The cached value and its generation stamp, moving the entry to the
    /// most recently used position (`inst-os-algo-lru-3`).
    pub(crate) fn get(&self, key: &str) -> Option<(V, u64)> {
        let mut guard = self.entries.lock();
        guard.1 += 1;
        let now = guard.1;
        let entry = guard.0.get_mut(key)?;
        entry.last_used = now;
        Some((entry.value.clone(), entry.generation))
    }

    /// Insert `value`, stamped with `generation`, evicting the least recently
    /// used entry at capacity (`inst-os-algo-lru-5`).
    pub(crate) fn insert(&self, key: &str, value: V, generation: u64) {
        let mut guard = self.entries.lock();
        guard.1 += 1;
        let now = guard.1;
        if guard.0.len() >= self.capacity && !guard.0.contains_key(key) {
            if let Some(oldest) = guard
                .0
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone())
            {
                guard.0.remove(&oldest);
            }
        }
        guard.0.insert(key.to_owned(), Entry { value, generation, last_used: now });
    }

    /// Remove `key` (`inst-os-cpinv-4b`).
    pub(crate) fn remove(&self, key: &str) {
        self.entries.lock().0.remove(key);
    }

    /// Remove every entry the predicate declines; the predicate sees the key
    /// and the entry and runs under the guard, so it must not re-enter.
    pub(crate) fn retain_entries(&self, keep: impl Fn(&str, &V) -> bool) {
        self.entries.lock().0.retain(|key, entry| keep(key, &entry.value));
    }

    /// Drop every entry — the flush fallback of `inst-os-algo-inval-8`.
    pub(crate) fn clear(&self) {
        self.entries.lock().0.clear();
    }

    /// How many entries the cache holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.lock().0.len()
    }

    /// Whether `key` is cached, without refreshing its recency, so an
    /// integration test can assert the population and the invalidation of one
    /// entry.
    #[must_use]
    pub fn contains(&self, key: &str) -> bool {
        self.entries.lock().0.contains_key(key)
    }

    /// Whether the cache holds no entry.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.lock().0.is_empty()
    }
}

/// The four outcomes of a Control Plane configuration read
/// (`inst-os-cpread-10`).
#[derive(Debug, Clone, PartialEq)]
pub enum CachedRead<V> {
    /// The cached value, served without consulting the repository.
    Cached(V),
    /// The resolved value, populated on the way out.
    Resolved(V),
    /// The store holds no record: nothing is cached.
    NotFound(DomainError),
    /// The store read failed: entry 2.5 classifies it, nothing is cached.
    StoreError(DomainError),
}

impl<V> CachedRead<V> {
    /// The value of a cached or resolved read.
    #[must_use]
    pub fn value(&self) -> Option<&V> {
        match self {
            Self::Cached(value) | Self::Resolved(value) => Some(value),
            Self::NotFound(_) | Self::StoreError(_) => None,
        }
    }

    /// Whether the read was served from the cache.
    #[must_use]
    pub const fn was_cached(&self) -> bool {
        matches!(self, Self::Cached(_))
    }
}

/// The Control Plane state this entry owns: the L1 cache and the shared L2
/// layer it documents but does not implement
/// (`cpt-cf-oagw-dod-observability-and-state-cp-cache`,
/// `cpt-cf-oagw-dod-observability-and-state-deployment-modes`).
pub struct CPState {
    /// The 10,000-entry L1 LRU.
    pub l1: Arc<L1<UpstreamRecord>>,
    /// `None`: no L2 layer exists in the graded configuration
    /// (`inst-os-deploy-2`).
    pub l2_cache: Option<Arc<dyn L2Cache>>,
}

/// The optional shared L2 cache of ADR 0005, which the microservice mode
/// documents and the graded configuration does not implement
/// (`cpt-cf-oagw-dod-observability-and-state-deployment-modes`).
impl Clone for CPState {
    fn clone(&self) -> Self {
        Self { l1: Arc::clone(&self.l1), l2_cache: self.l2_cache.clone() }
    }
}

impl std::fmt::Debug for CPState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CPState")
            .field("l1", &self.l1)
            .field("l2_cache", &self.l2_cache.as_ref().map(|_| "reserved"))
            .finish()
    }
}

/// The optional shared L2 cache of ADR 0005, which the microservice mode
/// documents and the graded configuration does not implement
/// (`cpt-cf-oagw-dod-observability-and-state-deployment-modes`).
pub trait L2Cache: Send + Sync {
    /// Whether this layer is available.
    fn available(&self) -> bool;
}

/// Refuse the shared L2 layer rather than silently degrading to L1
/// (`inst-os-deploy-4`).
///
/// # Errors
///
/// Always: the graded configuration declares no L2 implementation.
pub fn refuse_l2() -> Result<(), DomainError> {
    Err(DomainError::Internal(
        "the shared L2 cache layer is not supported in the graded configuration".to_owned(),
    ))
}

impl CPState {
    /// The single-executable state: L1 only, sized from the fixed constant
    /// (`inst-os-deploy-1`, `inst-os-deploy-2b`).
    #[must_use]
    pub fn single_executable() -> Self {
        Self { l1: Arc::new(L1::new(CP_L1_CAPACITY)), l2_cache: None }
    }

    /// Construct a state that asks for the shared L2 layer: refused
    /// (`inst-os-deploy-4`).
    ///
    /// # Errors
    ///
    /// Always, with the unsupported-deployment error.
    pub fn with_l2(_l2: Arc<dyn L2Cache>) -> Result<Self, DomainError> {
        let error = refuse_l2().unwrap_err();
        Err(error)
    }

    /// The affected Control Plane keys of one written record
    /// (`inst-os-cpinv-4`, `cpt-cf-oagw-algo-observability-and-state-cache-key-derivation`).
    #[must_use]
    pub fn affected_keys(notification: &WriteNotification) -> Vec<CacheKey> {
        let mut keys = Vec::new();
        if let Some(alias) = &notification.upstream_alias {
            keys.push(CacheKey::Upstream {
                owner_tenant_id: notification.tenant_id,
                alias: alias.clone(),
            });
        }
        if let Some(written) = &notification.route {
            for method in &written.methods {
                keys.push(CacheKey::Route {
                    upstream_id: written.upstream_id,
                    method: method.clone(),
                    path_prefix: written.path_prefix.clone(),
                });
            }
        }
        if let Some(plugin_id) = notification.plugin_id {
            // `inst-os-algo-key-3`: the family is reserved and has no reader.
            keys.push(CacheKey::Plugin { plugin_id });
        }
        keys
    }

    /// Drop every entry: the fallback for a write whose affected key set
    /// cannot be derived (`inst-os-algo-inval-8`).
    pub fn flush_all(&self) {
        self.l1.clear();
    }

    /// Remove the affected keys from the L1 cache
    /// (`inst-os-cpinv-4b`), skipping the shared L2 flush when no L2 layer is
    /// constructed (`inst-os-cpinv-5`, `inst-os-algo-inval-4`).
    pub fn invalidate(&self, keys: &[CacheKey]) {
        for key in keys {
            self.l1.remove(&key.as_string());
        }
        if let Some(l2) = &self.l2_cache {
            let _ = l2.available();
        }
    }
}

/// The notification a management write hands the invalidation hook, with the
/// identifiers the key derivation needs. The domain's
/// [`ConfigWriteNotification`](crate::domain::services::management::ConfigWriteNotification)
/// carries the same fields; this alias keeps the derivation independent of the
/// domain type's evolution.
pub type WriteNotification = crate::domain::services::management::ConfigWriteNotification;

/// The caching decorator over the upstream repository
/// (`cpt-cf-oagw-dod-observability-and-state-cp-cache`).
///
/// The cache serves only `get_by_alias`, the one lookup the
/// `upstream:{owner_tenant_id}:{alias}` family can express; `get`, `list`,
/// and every write delegate untouched. The `tenant_id` component of the key is
/// the **owning** tenant, which is exactly the tenant the store lookup is
/// scoped to, so a descendant resolution that walked the chain to an ancestor
/// record is invalidated when that ancestor record is written
/// (`inst-os-algo-key-1`).
pub struct CachedUpstreamRepository {
    inner: Arc<dyn UpstreamRepository>,
    state: CPState,
    generations: ConfigGenerations,
}

impl std::fmt::Debug for CachedUpstreamRepository {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CachedUpstreamRepository")
    }
}

impl CachedUpstreamRepository {
    /// Decorate `inner` with the L1 cache of `state`.
    #[must_use]
    pub fn new(inner: Arc<dyn UpstreamRepository>, state: CPState, generations: ConfigGenerations) -> Self {
        Self { inner, state, generations }
    }

    /// The decorated repository, for a write path that must reach the store
    /// directly.
    #[must_use]
    pub fn inner(&self) -> &Arc<dyn UpstreamRepository> {
        &self.inner
    }

    /// The state whose L1 cache this decorator maintains.
    #[must_use]
    pub const fn state(&self) -> &CPState {
        &self.state
    }
}

impl UpstreamRepository for CachedUpstreamRepository {
    /// The tenant-scoped read of one record, through the L1 cache
    /// (`inst-os-cpread-1` .. `-10`).
    fn get(&self, tenant_id: Uuid, id: Uuid) -> Result<UpstreamRecord, DomainError> {
        self.inner.get(tenant_id, id)
    }

    /// The alias lookup one alias-resolution step performs: the one read the
    /// `upstream:{owner_tenant_id}:{alias}` family expresses.
    fn get_by_alias(&self, tenant_id: Uuid, alias: &str) -> Result<UpstreamRecord, DomainError> {
        let key = CacheKey::Upstream { owner_tenant_id: tenant_id, alias: alias.to_owned() };
        let rendered = key.as_string();
        if let Some((value, generation)) = self.state.l1.get(&rendered) {
            let current = self.generations.generation(&rendered);
            if generation == current {
                return Ok(value);
            }
            // `inst-os-algo-lru-4b`: a stale generation is a miss, and the
            // entry is discarded rather than re-inserted.
        }
        // The generation is captured before the store read and the insert is
        // accepted only while it still equals the store's current generation,
        // so a population that raced a flush cannot insert a pre-write value
        // (`inst-os-algo-lru-4`).
        let observed = self.generations.generation(&rendered);
        match self.inner.get_by_alias(tenant_id, alias) {
            Ok(record) => {
                if observed == self.generations.generation(&rendered) {
                    self.state.l1.insert(&rendered, record.clone(), observed);
                }
                Ok(record)
            }
            Err(error) if error.is_not_found() => Err(error),
            Err(error) => Err(error),
        }
    }

    /// A list lookup is not one of the three families: it bypasses the cache.
    fn list(&self, tenant_id: Uuid) -> Result<Vec<UpstreamRecord>, DomainError> {
        self.inner.list(tenant_id)
    }

    fn create(&self, tenant_id: Uuid, record: UpstreamRecord) -> Result<UpstreamRecord, DomainError> {
        let written = self.inner.create(tenant_id, record);
        if written.is_ok() {
            self.generations.bump(&CacheKey::Upstream {
                owner_tenant_id: tenant_id,
                alias: written.as_ref().expect("the written record").upstream.alias.clone(),
            }
            .as_string());
        }
        written
    }

    fn replace(&self, tenant_id: Uuid, record: UpstreamRecord) -> Result<UpstreamRecord, DomainError> {
        let written = self.inner.replace(tenant_id, record);
        if written.is_ok() {
            self.generations.bump(&CacheKey::Upstream {
                owner_tenant_id: tenant_id,
                alias: written.as_ref().expect("the written record").upstream.alias.clone(),
            }
            .as_string());
        }
        written
    }

    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        let alias = self.inner.get(tenant_id, id).ok().map(|record| record.upstream.alias);
        let deleted = self.inner.delete(tenant_id, id);
        if deleted.is_ok() {
            if let Some(alias) = alias {
                self.generations.bump(
                    &CacheKey::Upstream { owner_tenant_id: tenant_id, alias }.as_string(),
                );
            }
        }
        deleted
    }
}

#[cfg(test)]
#[path = "cp_cache_tests.rs"]
mod cp_cache_tests;
