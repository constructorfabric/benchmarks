//! In-memory repositories.
//!
//! Uses `dashmap` sharded maps plus a global `parking_lot::RwLock` for
//! multi-key consistency (alias uniqueness, match-rule uniqueness).

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use uuid::Uuid;

use crate::domain::error::{OagwError, OagwResult};
use crate::domain::model::{Plugin, Route, Upstream};
use crate::domain::repo::{
    PluginRepository, RouteRepository, UpstreamRepository,
};

/// Shared in-memory configuration store.
#[derive(Debug, Default)]
pub struct MemoryStore {
    upstreams: RwLock<HashMap<Uuid, Upstream>>,
    routes: RwLock<HashMap<Uuid, Route>>,
    plugins: RwLock<HashMap<String, Plugin>>,
}

impl MemoryStore {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Runs `f` with read access to upstreams.
    pub fn with_upstreams<R>(&self, f: impl FnOnce(&HashMap<Uuid, Upstream>) -> R) -> R {
        let guard = self.upstreams.read();
        f(&guard)
    }

    /// Runs `f` with write access to upstreams.
    pub fn with_upstreams_mut<R>(&self, f: impl FnOnce(&mut HashMap<Uuid, Upstream>) -> R) -> R {
        let mut guard = self.upstreams.write();
        f(&mut guard)
    }

    /// Runs `f` with read access to routes.
    pub fn with_routes<R>(&self, f: impl FnOnce(&HashMap<Uuid, Route>) -> R) -> R {
        let guard = self.routes.read();
        f(&guard)
    }

    /// Runs `f` with write access to routes.
    pub fn with_routes_mut<R>(&self, f: impl FnOnce(&mut HashMap<Uuid, Route>) -> R) -> R {
        let mut guard = self.routes.write();
        f(&mut guard)
    }

    /// Runs `f` with read access to plugins.
    pub fn with_plugins<R>(&self, f: impl FnOnce(&HashMap<String, Plugin>) -> R) -> R {
        let guard = self.plugins.read();
        f(&guard)
    }

    /// Runs `f` with write access to plugins.
    pub fn with_plugins_mut<R>(&self, f: impl FnOnce(&mut HashMap<String, Plugin>) -> R) -> R {
        let mut guard = self.plugins.write();
        f(&mut guard)
    }
}

impl UpstreamRepository for MemoryStore {
    fn insert_upstream(&self, upstream: Upstream) -> OagwResult<()> {
        self.with_upstreams_mut(|map| {
            map.insert(upstream.id, upstream);
        });
        Ok(())
    }

    fn find_upstream(&self, id: Uuid) -> OagwResult<Option<Upstream>> {
        Ok(self.with_upstreams(|map| map.get(&id).cloned()))
    }

    fn find_upstream_by_alias(&self, tenant_id: Uuid, alias: &str) -> OagwResult<Option<Upstream>> {
        Ok(self.with_upstreams(|map| {
            map.values()
                .find(|u| u.tenant_id == tenant_id && u.alias == alias)
                .cloned()
        }))
    }

    fn upstreams_for_tenant(&self, tenant_id: Uuid) -> OagwResult<Vec<Upstream>> {
        Ok(self.with_upstreams(|map| {
            let mut all: Vec<Upstream> = map
                .values()
                .filter(|u| u.tenant_id == tenant_id)
                .cloned()
                .collect();
            all.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id)));
            all
        }))
    }

    fn update_upstream(&self, upstream: Upstream) -> OagwResult<()> {
        self.with_upstreams_mut(|map| {
            map.insert(upstream.id, upstream);
        });
        Ok(())
    }

    fn delete_upstream(&self, id: Uuid) -> OagwResult<()> {
        self.with_upstreams_mut(|map| {
            map.remove(&id);
        });
        Ok(())
    }

    fn upstream_route_count(&self, id: Uuid) -> OagwResult<usize> {
        Ok(self.with_routes(|map| map.values().filter(|r| r.upstream_id == id).count()))
    }
}

impl RouteRepository for MemoryStore {
    fn insert_route(&self, route: Route) -> OagwResult<()> {
        self.with_routes_mut(|map| {
            map.insert(route.id, route);
        });
        Ok(())
    }

    fn find_route(&self, id: Uuid) -> OagwResult<Option<Route>> {
        Ok(self.with_routes(|map| map.get(&id).cloned()))
    }

    fn routes_for_tenant(&self, tenant_id: Uuid) -> OagwResult<Vec<Route>> {
        Ok(self.with_routes(|map| {
            let mut all: Vec<Route> = map
                .values()
                .filter(|r| r.tenant_id == tenant_id)
                .cloned()
                .collect();
            all.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id)));
            all
        }))
    }

    fn routes_for_upstream(&self, upstream_id: Uuid) -> OagwResult<Vec<Route>> {
        Ok(self.with_routes(|map| {
            map.values()
                .filter(|r| r.upstream_id == upstream_id)
                .cloned()
                .collect()
        }))
    }

    fn update_route(&self, route: Route) -> OagwResult<()> {
        self.with_routes_mut(|map| {
            map.insert(route.id, route);
        });
        Ok(())
    }

    fn delete_route(&self, id: Uuid) -> OagwResult<()> {
        self.with_routes_mut(|map| {
            map.remove(&id);
        });
        Ok(())
    }
}

impl PluginRepository for MemoryStore {
    fn insert_plugin(&self, plugin: Plugin) -> OagwResult<()> {
        self.with_plugins_mut(|map| {
            map.insert(plugin.id.clone(), plugin);
        });
        Ok(())
    }

    fn find_plugin(&self, id: &str) -> OagwResult<Option<Plugin>> {
        Ok(self.with_plugins(|map| map.get(id).cloned()))
    }

    fn plugins_for_tenant(&self, tenant_id: Uuid) -> OagwResult<Vec<Plugin>> {
        Ok(self.with_plugins(|map| {
            let mut all: Vec<Plugin> = map
                .values()
                .filter(|p| p.tenant_id == tenant_id)
                .cloned()
                .collect();
            all.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id)));
            all
        }))
    }

    fn delete_plugin(&self, id: &str) -> OagwResult<()> {
        self.with_plugins_mut(|map| {
            map.remove(id);
        });
        Ok(())
    }
}

/// A not-found error for a named resource kind.
#[must_use]
pub fn not_found(kind: &str, id: &str) -> OagwError {
    OagwError::RouteNotFound(format!("{kind} '{id}' not found"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{Endpoint, EndpointScheme, UpstreamServer};

    fn upstream(tenant: Uuid, alias: &str) -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            enabled: true,
            alias: alias.to_owned(),
            alias_explicit: false,
            tags: Vec::new(),
            server: UpstreamServer {
                endpoints: vec![Endpoint {
                    scheme: EndpointScheme::Https,
                    host: "api.openai.com".to_owned(),
                    port: 443,
                }],
            },
            protocol: crate::domain::gts_helpers::PROTOCOL_HTTP.to_owned(),
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
            created_at: 1,
            updated_at: 1,
        }
    }

    #[test]
    fn stores_and_finds_by_alias() {
        let store = MemoryStore::new();
        let tenant = Uuid::new_v4();
        let record = upstream(tenant, "api.openai.com");
        let id = record.id;
        store.insert_upstream(record).unwrap();
        assert!(store.find_upstream(id).unwrap().is_some());
        assert!(
            store
                .find_upstream_by_alias(tenant, "api.openai.com")
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .find_upstream_by_alias(Uuid::new_v4(), "api.openai.com")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn delete_removes_record() {
        let store = MemoryStore::new();
        let tenant = Uuid::new_v4();
        let record = upstream(tenant, "x");
        let id = record.id;
        store.insert_upstream(record).unwrap();
        store.delete_upstream(id).unwrap();
        assert!(store.find_upstream(id).unwrap().is_none());
    }
}
