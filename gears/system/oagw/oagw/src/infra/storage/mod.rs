//! In-memory repository implementations on DashMap (feature
//! `cpt-cf-oagw-feature-domain-model-repositories`).
//!
//! Backs the §3.7 relational table shapes with thread-safe DashMap storage
//! (constraint `cpt-cf-oagw-constraint-in-memory-storage`, flow
//! `cpt-cf-oagw-flow-domain-model-repositories-persist`).  No external
//! database dependency: this delivery persists configuration through the
//! repository traits; a future SeaORM/`toolkit-db` swap satisfies the same
//! traits (DoD `cpt-cf-oagw-dod-domain-model-repositories-repo-traits`,
//! `cpt-cf-oagw-dod-domain-model-repositories-tenant-scope`).
//!
//! A single [`InMemoryStore`] holds all tables so cross-repository invariants
//! (route `upstream_id` cascade delete, plugin in-use lookup across binding
//! rows) hold atomically under one internal mutex.

use std::sync::Arc;

use dashmap::DashMap;
use uuid::Uuid;

use crate::domain::entity::{Plugin, Route, Upstream};
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};

mod plugin;
mod route;
mod upstream;

pub use plugin::InMemoryPluginRepo;
pub use route::InMemoryRouteRepo;
pub use upstream::InMemoryUpstreamRepo;
pub use upstream::UpstreamRepoOptions;

/// Shared in-memory tables (mirrors the `oagw_*` relational shapes).
#[derive(Debug, Default)]
pub(crate) struct StoreTables {
    /// `oagw_upstream` rows keyed by `(tenant_id, id)`.
    upstreams: DashMap<(Uuid, Uuid), Upstream>,
    /// `(tenant_id, alias)` → `id` (UNIQUE).
    upstream_alias: DashMap<(Uuid, String), Uuid>,
    /// `oagw_route` rows keyed by `(tenant_id, id)`.
    routes: DashMap<(Uuid, Uuid), Route>,
    /// `(tenant_id, upstream_id)` → route ids (FK index w/ cascade).
    route_upstream: DashMap<(Uuid, Uuid), Vec<Uuid>>,
    /// `oagw_plugin` rows keyed by `(tenant_id, id)`.
    plugins: DashMap<(Uuid, Uuid), Plugin>,
    /// `(tenant_id, name)` → `id` (UNIQUE).
    plugin_name: DashMap<(Uuid, String), Uuid>,
}

/// The in-memory storage root; repository handles are cheap views over it.
#[derive(Debug, Default)]
pub struct InMemoryStore {
    tables: Arc<StoreTables>,
    /// Serializes multi-table mutations so they are atomic (mirrors the
    /// multi-table-atomic invariant of `cpt-cf-oagw-db-schema`).
    lock: Arc<parking_lot::Mutex<()>>,
}

impl InMemoryStore {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the upstream repository view.
    #[must_use]
    pub fn upstream_repo(&self, options: UpstreamRepoOptions) -> Arc<dyn UpstreamRepository> {
        Arc::new(InMemoryUpstreamRepo::new(
            Arc::clone(&self.tables),
            Arc::clone(&self.lock),
            options,
        ))
    }

    /// Returns the route repository view.
    #[must_use]
    pub fn route_repo(&self) -> Arc<dyn RouteRepository> {
        Arc::new(InMemoryRouteRepo::new(
            Arc::clone(&self.tables),
            Arc::clone(&self.lock),
        ))
    }

    /// Returns the plugin repository view (also the plugin registry).
    #[must_use]
    pub fn plugin_repo(&self) -> Arc<dyn PluginRepository> {
        Arc::new(InMemoryPluginRepo::new(
            Arc::clone(&self.tables),
            Arc::clone(&self.lock),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_hands_out_typed_repo_handles() {
        let store = InMemoryStore::new();
        let _up: Arc<dyn UpstreamRepository> = store.upstream_repo(UpstreamRepoOptions::default());
        let _rt: Arc<dyn RouteRepository> = store.route_repo();
        let _pl: Arc<dyn PluginRepository> = store.plugin_repo();
    }
}
