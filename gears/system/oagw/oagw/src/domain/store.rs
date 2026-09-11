//! In-memory configuration store.
//!
//! The gear has no database dependency: upstream, route and plugin
//! definitions live in process memory for the lifetime of the server. All
//! access goes through a single [`parking_lot::RwLock`], so the control plane
//! stays linearizable and the data plane sees a consistent snapshot per
//! request.

use parking_lot::RwLock;
use std::sync::Arc;
use uuid::Uuid;

use crate::domain::model::{PluginDefinition, Route, Upstream};

/// A shared handle to the store.
pub type SharedStore = Arc<Store>;

#[derive(Debug, Default)]
struct Inner {
    upstreams: Vec<Upstream>,
    routes: Vec<Route>,
    plugins: Vec<PluginDefinition>,
}

/// In-memory control-plane store.
#[derive(Debug, Default)]
pub struct Store {
    inner: RwLock<Inner>,
}

impl Store {
    /// Create an empty store.
    #[must_use]
    pub fn new() -> SharedStore {
        Arc::new(Self::default())
    }

    // -- upstreams ---------------------------------------------------------

    /// Insert an upstream definition.
    pub fn insert_upstream(&self, upstream: Upstream) {
        self.inner.write().upstreams.push(upstream);
    }

    /// Look up an upstream by id.
    #[must_use]
    pub fn get_upstream(&self, id: &Uuid) -> Option<Upstream> {
        self.inner
            .read()
            .upstreams
            .iter()
            .find(|upstream| &upstream.id == id)
            .cloned()
    }

    /// All upstreams owned by `tenant_id`, in insertion order.
    #[must_use]
    pub fn list_upstreams(&self, tenant_id: &Uuid) -> Vec<Upstream> {
        self.inner
            .read()
            .upstreams
            .iter()
            .filter(|upstream| &upstream.tenant_id == tenant_id)
            .cloned()
            .collect()
    }

    /// The first upstream of `tenant_id` whose alias matches, case-insensitively.
    #[must_use]
    pub fn find_upstream_by_alias(&self, tenant_id: &Uuid, alias: &str) -> Option<Upstream> {
        self.inner
            .read()
            .upstreams
            .iter()
            .find(|upstream| {
                &upstream.tenant_id == tenant_id && upstream.alias.eq_ignore_ascii_case(alias)
            })
            .cloned()
    }

    /// Whether `(tenant_id, alias)` is already taken.
    #[must_use]
    pub fn alias_taken(&self, tenant_id: &Uuid, alias: &str) -> bool {
        self.inner.read().upstreams.iter().any(|upstream| {
            &upstream.tenant_id == tenant_id && upstream.alias.eq_ignore_ascii_case(alias)
        })
    }

    /// Replace an upstream definition, keeping its creation timestamp.
    ///
    /// # Errors
    ///
    /// Errors when the id is unknown.
    pub fn update_upstream(&self, upstream: Upstream) -> Result<(), String> {
        let mut guard = self.inner.write();
        let slot = guard
            .upstreams
            .iter_mut()
            .find(|existing| existing.id == upstream.id)
            .ok_or_else(|| "upstream not found".to_owned())?;
        let created_at = slot.created_at.take();
        *slot = upstream;
        slot.created_at = created_at;
        Ok(())
    }

    /// Delete an upstream, returning the removed definition.
    pub fn delete_upstream(&self, id: &Uuid) -> Option<Upstream> {
        let mut guard = self.inner.write();
        let index = guard
            .upstreams
            .iter()
            .position(|upstream| &upstream.id == id)?;
        Some(guard.upstreams.remove(index))
    }

    /// Every upstream in the store, across tenants.
    #[must_use]
    pub fn all_upstreams(&self) -> Vec<Upstream> {
        self.inner.read().upstreams.clone()
    }

    /// Upstreams that bind `plugin_ref`, either as an auth plugin or as a
    /// member of their plugin chain.
    #[must_use]
    pub fn upstreams_referencing(&self, plugin_ref: &str) -> Vec<Upstream> {
        self.inner
            .read()
            .upstreams
            .iter()
            .filter(|upstream| {
                let as_auth = upstream
                    .auth
                    .as_ref()
                    .is_some_and(|auth| auth.auth_type == plugin_ref);
                as_auth
                    || upstream
                        .plugins
                        .items
                        .iter()
                        .any(|b| b.plugin_ref == plugin_ref)
            })
            .cloned()
            .collect()
    }

    // -- routes ------------------------------------------------------------

    /// Insert a route definition.
    pub fn insert_route(&self, route: Route) {
        self.inner.write().routes.push(route);
    }

    /// Look up a route by id.
    #[must_use]
    pub fn get_route(&self, id: &Uuid) -> Option<Route> {
        self.inner
            .read()
            .routes
            .iter()
            .find(|route| &route.id == id)
            .cloned()
    }

    /// All routes owned by `tenant_id`, in insertion order.
    #[must_use]
    pub fn list_routes(&self, tenant_id: &Uuid) -> Vec<Route> {
        self.inner
            .read()
            .routes
            .iter()
            .filter(|route| &route.tenant_id == tenant_id)
            .cloned()
            .collect()
    }

    /// Routes of `tenant_id` that target `upstream_id`.
    #[must_use]
    pub fn routes_for_upstream(&self, tenant_id: &Uuid, upstream_id: &Uuid) -> Vec<Route> {
        self.inner
            .read()
            .routes
            .iter()
            .filter(|route| &route.tenant_id == tenant_id && &route.upstream_id == upstream_id)
            .cloned()
            .collect()
    }

    /// Replace a route definition.
    ///
    /// # Errors
    ///
    /// Errors when the route id is unknown.
    pub fn update_route(&self, route: Route) -> Result<(), String> {
        let mut guard = self.inner.write();
        let slot = guard
            .routes
            .iter_mut()
            .find(|existing| existing.id == route.id)
            .ok_or_else(|| "route not found".to_owned())?;
        let created_at = slot.created_at.take();
        *slot = route;
        slot.created_at = created_at;
        Ok(())
    }

    /// Delete a route, returning the removed definition.
    pub fn delete_route(&self, id: &Uuid) -> Option<Route> {
        let mut guard = self.inner.write();
        let index = guard.routes.iter().position(|route| &route.id == id)?;
        Some(guard.routes.remove(index))
    }

    /// Every route in the store, across tenants.
    #[must_use]
    pub fn all_routes(&self) -> Vec<Route> {
        self.inner.read().routes.clone()
    }

    /// Routes that reference `plugin_ref`.
    #[must_use]
    pub fn routes_referencing(&self, plugin_ref: &str) -> Vec<Route> {
        self.inner
            .read()
            .routes
            .iter()
            .filter(|route| {
                route
                    .plugins
                    .items
                    .iter()
                    .any(|b| b.plugin_ref == plugin_ref)
            })
            .cloned()
            .collect()
    }

    // -- plugins -----------------------------------------------------------

    /// Insert a plugin definition.
    pub fn insert_plugin(&self, plugin: PluginDefinition) {
        self.inner.write().plugins.push(plugin);
    }

    /// Look up a plugin definition by id.
    #[must_use]
    pub fn get_plugin(&self, id: &Uuid) -> Option<PluginDefinition> {
        self.inner
            .read()
            .plugins
            .iter()
            .find(|plugin| &plugin.id == id)
            .cloned()
    }

    /// Look up a plugin definition by its GTS instance id.
    #[must_use]
    pub fn get_plugin_by_ref(&self, plugin_ref: &str) -> Option<PluginDefinition> {
        self.inner
            .read()
            .plugins
            .iter()
            .find(|plugin| plugin.plugin_ref == plugin_ref)
            .cloned()
    }

    /// All plugins owned by `tenant_id`.
    #[must_use]
    pub fn list_plugins(&self, tenant_id: &Uuid) -> Vec<PluginDefinition> {
        self.inner
            .read()
            .plugins
            .iter()
            .filter(|plugin| &plugin.tenant_id == tenant_id)
            .cloned()
            .collect()
    }

    /// Delete a plugin, returning the removed definition.
    pub fn delete_plugin(&self, id: &Uuid) -> Option<PluginDefinition> {
        let mut guard = self.inner.write();
        let index = guard.plugins.iter().position(|plugin| &plugin.id == id)?;
        Some(guard.plugins.remove(index))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{Endpoint, HttpMatch, MatchRule, Scheme};

    fn upstream(tenant: Uuid, alias: &str) -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            enabled: true,
            alias: alias.to_owned(),
            tags: vec![],
            server: crate::domain::model::ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: Scheme::Https,
                    host: "api.example.test".to_owned(),
                    port: 443,
                }],
            },
            protocol: crate::domain::model::Protocol::Http,
            auth: None,
            headers: Default::default(),
            plugins: Default::default(),
            rate_limit: None,
            cors: None,
            created_at: None,
            updated_at: None,
        }
    }

    fn route(tenant: Uuid, upstream: Uuid, path: &str) -> Route {
        Route {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            enabled: true,
            tags: vec![],
            upstream_id: upstream,
            match_rule: MatchRule::Http(HttpMatch {
                methods: vec!["GET".to_owned()],
                path: path.to_owned(),
                query_allowlist: vec![],
                path_suffix_mode: crate::domain::model::PathSuffixMode::Append,
            }),
            plugins: Default::default(),
            rate_limit: None,
            created_at: None,
            updated_at: None,
        }
    }

    #[test]
    fn upstreams_are_tenant_scoped() {
        let store = Store::new();
        let tenant_a = Uuid::new_v4();
        let tenant_b = Uuid::new_v4();
        let a = upstream(tenant_a, "api.example.test");
        store.insert_upstream(a.clone());
        store.insert_upstream(upstream(tenant_b, "other.example.test"));

        assert_eq!(store.list_upstreams(&tenant_a).len(), 1);
        assert!(store.alias_taken(&tenant_a, "API.EXAMPLE.TEST"));
        assert!(!store.alias_taken(&tenant_b, "api.example.test"));
        assert_eq!(
            store
                .find_upstream_by_alias(&tenant_a, "api.example.test")
                .map(|u| u.id),
            Some(a.id)
        );
        assert_eq!(
            store.find_upstream_by_alias(&tenant_b, "api.example.test"),
            None
        );
    }

    #[test]
    fn update_and_delete_round_trip() {
        let tenant = Uuid::new_v4();
        let store = Store::new();
        let mut a = upstream(tenant, "a.example.test");
        store.insert_upstream(a.clone());

        a.enabled = false;
        store.update_upstream(a.clone()).expect("update");
        assert!(!store.get_upstream(&a.id).expect("present").enabled);

        assert_eq!(store.delete_upstream(&a.id).map(|u| u.id), Some(a.id));
        assert!(store.get_upstream(&a.id).is_none());
        assert!(store.update_upstream(a).is_err());
    }

    #[test]
    fn routes_track_their_upstream() {
        let tenant = Uuid::new_v4();
        let store = Store::new();
        let a = upstream(tenant, "a.example.test");
        store.insert_upstream(a.clone());
        store.insert_route(route(tenant, a.id, "/v1"));

        assert_eq!(store.routes_for_upstream(&tenant, &a.id).len(), 1);
        assert_eq!(store.routes_referencing("nope").len(), 0);
        assert_eq!(store.list_routes(&tenant).len(), 1);
    }
}
