//! In-memory `OagwRepository` implementation.
//!
//! Per-tenant isolated stores guarded by a `parking_lot::RwLock`.  This is the
//! MVP persistence: entities live for the process lifetime.  A DB-backed
//! implementation can implement the same trait later without touching the
//! service layer.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use uuid::Uuid;

use crate::domain::dto::{CustomPlugin, Route, Upstream};
use crate::domain::repo::{DeleteOutcome, OagwRepository};

#[derive(Default)]
struct TenantStore {
    upstreams: HashMap<Uuid, Upstream>,
    upstreams_by_alias: HashMap<String, Uuid>,
    routes: HashMap<Uuid, Route>,
    plugins: HashMap<Uuid, CustomPlugin>,
}

#[derive(Default)]
struct Store {
    tenants: HashMap<Uuid, TenantStore>,
}

/// Synchronous in-memory repository.  All operations lock the tenant store
/// briefly; reads and writes are exclusive per tenant (fine for the MVP).
#[derive(Clone, Default)]
pub struct InMemoryRepository {
    inner: Arc<RwLock<Store>>,
}

impl InMemoryRepository {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

// NOTE: `tenant_mut` above borrows the outer guard; public methods re-lock for
// each operation to keep Rust borrow rules simple.  Each method locks the
// whole store briefly.
impl OagwRepository for InMemoryRepository {
    // ---- upstreams ----
    fn list_upstreams(&self, tenant_id: Uuid) -> Vec<Upstream> {
        let store = self.inner.read();
        store
            .tenants
            .get(&tenant_id)
            .map(|t| t.upstreams.values().cloned().collect())
            .unwrap_or_default()
    }

    fn get_upstream(&self, tenant_id: Uuid, id: Uuid) -> Option<Upstream> {
        let store = self.inner.read();
        store
            .tenants
            .get(&tenant_id)
            .and_then(|t| t.upstreams.get(&id).cloned())
    }

    fn find_upstream_by_alias(&self, tenant_id: Uuid, alias: &str) -> Option<Upstream> {
        let store = self.inner.read();
        store
            .tenants
            .get(&tenant_id)
            .and_then(|t| t.upstreams_by_alias.get(alias))
            .and_then(|id| {
                store
                    .tenants
                    .get(&tenant_id)
                    .and_then(|t| t.upstreams.get(id).cloned())
            })
    }

    fn insert_upstream(&self, upstream: Upstream) {
        let mut store = self.inner.write();
        let tenant = store.tenants.entry(upstream.tenant_id).or_default();
        tenant
            .upstreams_by_alias
            .insert(upstream.alias.clone(), upstream.id);
        tenant.upstreams.insert(upstream.id, upstream);
    }

    fn update_upstream(&self, upstream: Upstream) {
        let mut store = self.inner.write();
        if let Some(tenant) = store.tenants.get_mut(&upstream.tenant_id)
            && tenant.upstreams.contains_key(&upstream.id)
        {
            // refresh alias index (alias is immutable in practice)
            tenant
                .upstreams_by_alias
                .insert(upstream.alias.clone(), upstream.id);
            tenant.upstreams.insert(upstream.id, upstream);
        }
    }

    fn remove_upstream(&self, tenant_id: Uuid, id: Uuid) -> DeleteOutcome {
        let mut store = self.inner.write();
        let Some(tenant) = store.tenants.get_mut(&tenant_id) else {
            return DeleteOutcome::NotFound;
        };
        match tenant.upstreams.remove(&id) {
            Some(up) => {
                tenant.upstreams_by_alias.remove(&up.alias);
                DeleteOutcome::Deleted
            }
            None => DeleteOutcome::NotFound,
        }
    }

    // ---- routes ----
    fn list_routes(&self, tenant_id: Uuid) -> Vec<Route> {
        let store = self.inner.read();
        store
            .tenants
            .get(&tenant_id)
            .map(|t| t.routes.values().cloned().collect())
            .unwrap_or_default()
    }

    fn list_routes_for_upstream(&self, tenant_id: Uuid, upstream_id: Uuid) -> Vec<Route> {
        let store = self.inner.read();
        store
            .tenants
            .get(&tenant_id)
            .map(|t| {
                t.routes
                    .values()
                    .filter(|r| r.upstream_id == upstream_id)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    fn get_route(&self, tenant_id: Uuid, id: Uuid) -> Option<Route> {
        let store = self.inner.read();
        store
            .tenants
            .get(&tenant_id)
            .and_then(|t| t.routes.get(&id).cloned())
    }

    fn insert_route(&self, route: Route) {
        let mut store = self.inner.write();
        store
            .tenants
            .entry(route.tenant_id)
            .or_default()
            .routes
            .insert(route.id, route);
    }

    fn update_route(&self, route: Route) {
        let mut store = self.inner.write();
        if let Some(tenant) = store.tenants.get_mut(&route.tenant_id) {
            tenant.routes.insert(route.id, route);
        }
    }

    fn remove_route(&self, tenant_id: Uuid, id: Uuid) -> DeleteOutcome {
        let mut store = self.inner.write();
        match store
            .tenants
            .get_mut(&tenant_id)
            .and_then(|t| t.routes.remove(&id))
        {
            Some(_) => DeleteOutcome::Deleted,
            None => DeleteOutcome::NotFound,
        }
    }

    // ---- custom plugins ----
    fn list_plugins(&self, tenant_id: Uuid) -> Vec<CustomPlugin> {
        let store = self.inner.read();
        store
            .tenants
            .get(&tenant_id)
            .map(|t| t.plugins.values().cloned().collect())
            .unwrap_or_default()
    }

    fn get_plugin(&self, tenant_id: Uuid, id: Uuid) -> Option<CustomPlugin> {
        let store = self.inner.read();
        store
            .tenants
            .get(&tenant_id)
            .and_then(|t| t.plugins.get(&id).cloned())
    }

    fn insert_plugin(&self, plugin: CustomPlugin) {
        let mut store = self.inner.write();
        store
            .tenants
            .entry(plugin.tenant_id)
            .or_default()
            .plugins
            .insert(plugin.id, plugin);
    }

    fn remove_plugin(&self, tenant_id: Uuid, id: Uuid) -> DeleteOutcome {
        let mut store = self.inner.write();
        match store
            .tenants
            .get_mut(&tenant_id)
            .and_then(|t| t.plugins.remove(&id))
        {
            Some(_) => DeleteOutcome::Deleted,
            None => DeleteOutcome::NotFound,
        }
    }

    /// UUIDs + full plugin refs of every bound plugin, per upstream.
    fn upstream_plugin_refs(&self, tenant_id: Uuid) -> Vec<(Uuid, String, Option<Uuid>)> {
        let store = self.inner.read();
        store
            .tenants
            .get(&tenant_id)
            .map(|t| {
                t.upstreams
                    .values()
                    .flat_map(|up| {
                        up.plugins
                            .items
                            .iter()
                            .map(move |b| (up.id, b.plugin_ref.clone(), Some(up.id)))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn tenant() -> Uuid {
        Uuid::nil()
    }

    fn upstream(id: u64) -> Upstream {
        Upstream {
            id: Uuid::from_u64_pair(0, id),
            tenant_id: tenant(),
            enabled: true,
            alias: format!("svc-{id}"),
            tags: vec![],
            server: crate::domain::dto::ServerConfig::default(),
            protocol: crate::domain::dto::Protocol::Http,
            auth: crate::domain::dto::AuthConfig::default(),
            headers: crate::domain::dto::HeadersConfig::default(),
            plugins: crate::domain::dto::PluginsConfig::default(),
            rate_limit: None,
            cors: None,
            created_at: 0,
        }
    }

    #[test]
    fn tenant_scopes_are_isolated() {
        let repo = InMemoryRepository::new();
        let other = Uuid::from_u64_pair(1, 1);
        repo.insert_upstream(upstream(1));
        assert_eq!(repo.list_upstreams(tenant()).len(), 1);
        assert!(repo.list_upstreams(other).is_empty());
    }

    #[test]
    fn alias_index_round_trips() {
        let repo = InMemoryRepository::new();
        repo.insert_upstream(upstream(7));
        let found = repo.find_upstream_by_alias(tenant(), "svc-7").unwrap();
        assert_eq!(found.id, Uuid::from_u64_pair(0, 7));
        assert_eq!(
            repo.remove_upstream(tenant(), found.id),
            DeleteOutcome::Deleted
        );
        assert!(repo.find_upstream_by_alias(tenant(), "svc-7").is_none());
    }

    #[test]
    fn routes_and_plugins_insert_and_remove() {
        let repo = InMemoryRepository::new();
        let route = Route {
            id: Uuid::from_u64_pair(0, 1),
            tenant_id: tenant(),
            enabled: true,
            upstream_id: Uuid::from_u64_pair(0, 7),
            priority: 0,
            match_config: crate::domain::dto::MatchConfig::default(),
            tags: vec![],
            plugins: crate::domain::dto::PluginsConfig::default(),
            rate_limit: None,
            cors: None,
            created_at: 0,
        };
        repo.insert_route(route.clone());
        assert_eq!(repo.list_routes(tenant()).len(), 1);
        assert_eq!(repo.get_route(tenant(), route.id).unwrap(), route);
        assert_eq!(
            repo.remove_route(tenant(), route.id),
            DeleteOutcome::Deleted
        );

        let plugin = CustomPlugin {
            id: Uuid::from_u64_pair(0, 2),
            tenant_id: tenant(),
            plugin_type: crate::domain::dto::PluginKind::Guard,
            name: "p".into(),
            description: None,
            config_schema: serde_json::Value::Null,
            source_code: String::new(),
            created_at: 0,
        };
        repo.insert_plugin(plugin.clone());
        assert_eq!(repo.get_plugin(tenant(), plugin.id).unwrap(), plugin);
        assert_eq!(repo.list_plugins(tenant()).len(), 1);
        assert_eq!(
            repo.remove_plugin(tenant(), plugin.id),
            DeleteOutcome::Deleted
        );
    }
}
