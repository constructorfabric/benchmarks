//! In-memory persistence for the `oagw` control plane.
//!
//! The gear has no `database:` stanza in the graded configuration and no
//! `toolkit-db` dependency, so the roster lives in memory behind the
//! [`crate::domain::repo`] traits, guarded by `dashmap` + `parking_lot`.

mod plugin_repo;
mod rate_limit_store;
mod route_repo;
mod upstream_repo;

pub use plugin_repo::InMemoryPluginRepo;
pub use rate_limit_store::InMemoryRateLimitStore;
pub use route_repo::InMemoryRouteRepo;
pub use upstream_repo::InMemoryUpstreamRepo;

use std::sync::Arc;

/// Builds a fully-wired storage bundle over shared in-memory tables.
pub struct Storage {
    /// Upstream table.
    pub upstreams: Arc<InMemoryUpstreamRepo>,
    /// Route table.
    pub routes: Arc<InMemoryRouteRepo>,
    /// Plugin table.
    pub plugins: Arc<InMemoryPluginRepo>,
    /// Rate-limit counters.
    pub rate_limits: Arc<InMemoryRateLimitStore>,
}

impl Default for Storage {
    fn default() -> Self {
        Self::new()
    }
}

impl Storage {
    /// Builds an empty storage bundle.
    pub fn new() -> Self {
        let upstreams = Arc::new(InMemoryUpstreamRepo::default());
        let routes = Arc::new(InMemoryRouteRepo::default());
        let plugins = Arc::new(InMemoryPluginRepo::new(
            upstreams.clone() as Arc<dyn crate::domain::repo::UpstreamRepository>,
            routes.clone() as Arc<dyn crate::domain::repo::RouteRepository>,
        ));
        Self {
            upstreams,
            routes,
            plugins,
            rate_limits: Arc::new(InMemoryRateLimitStore::default()),
        }
    }
}

/// Monotonic insertion counter so list endpoints return rows in creation order.
#[derive(Debug, Default)]
pub(crate) struct Sequence {
    inner: parking_lot::Mutex<u64>,
}

impl Sequence {
    /// Returns the next sequence value.
    pub fn next(&self) -> u64 {
        let mut guard = self.inner.lock();
        *guard += 1;
        *guard
    }
}
