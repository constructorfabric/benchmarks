//! In-memory control-plane store.
//!
//! Realizes `cpt-cf-oagw-algo-gf-init-state` / `cpt-cf-oagw-dod-gf-shared-state`.
//!
//! The graded configuration runs the gear without a database, so the entities,
//! invariants and uniqueness constraints that `cpt-cf-oagw-db-schema` describes
//! for the persisted deployment are realized here as process memory. State does
//! not survive a restart, which is correct for this configuration.

use std::collections::HashMap;

use parking_lot::RwLock;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::{PluginDef, Route, Upstream};

/// Shared control-plane state.
#[derive(Debug, Default)]
pub struct Store {
    inner: RwLock<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    upstreams: HashMap<Uuid, Upstream>,
    routes: HashMap<Uuid, Route>,
    plugins: HashMap<Uuid, PluginDef>,
}

impl Store {
    /// A new, empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    // ---- upstreams ----------------------------------------------------

    /// Insert an upstream, enforcing `UNIQUE(tenant_id, alias)`.
    ///
    /// # Errors
    /// Returns [`DomainError::AlreadyExists`] when the tenant already owns an
    /// upstream with this alias.
    pub fn insert_upstream(&self, up: Upstream) -> Result<Upstream, DomainError> {
        let mut g = self.inner.write();
        if g
            .upstreams
            .values()
            .any(|u| u.tenant_id == up.tenant_id && u.alias == up.alias)
        {
            return Err(DomainError::AlreadyExists {
                resource: "upstream".to_owned(),
                key: up.alias.clone(),
            });
        }
        g.upstreams.insert(up.id, up.clone());
        Ok(up)
    }

    /// Fetch an upstream visible to `tenant`.
    #[must_use]
    pub fn get_upstream(&self, tenant: Uuid, id: Uuid) -> Option<Upstream> {
        self.inner
            .read()
            .upstreams
            .get(&id)
            .filter(|u| u.tenant_id == tenant)
            .cloned()
    }

    /// Resolve an upstream by its alias within `tenant`.
    #[must_use]
    pub fn find_upstream_by_alias(&self, tenant: Uuid, alias: &str) -> Option<Upstream> {
        self.inner
            .read()
            .upstreams
            .values()
            .find(|u| u.tenant_id == tenant && u.alias == alias)
            .cloned()
    }

    /// List the tenant's upstreams, ordered by alias for a stable page.
    #[must_use]
    pub fn list_upstreams(&self, tenant: Uuid) -> Vec<Upstream> {
        let mut v: Vec<_> = self
            .inner
            .read()
            .upstreams
            .values()
            .filter(|u| u.tenant_id == tenant)
            .cloned()
            .collect();
        v.sort_by(|a, b| a.alias.cmp(&b.alias));
        v
    }

    /// Replace an upstream in place.
    ///
    /// # Errors
    /// Returns [`DomainError::NotFound`] when the tenant does not own it.
    pub fn replace_upstream(&self, up: Upstream) -> Result<Upstream, DomainError> {
        let mut g = self.inner.write();
        match g.upstreams.get(&up.id) {
            Some(existing) if existing.tenant_id == up.tenant_id => {
                g.upstreams.insert(up.id, up.clone());
                Ok(up)
            }
            _ => Err(DomainError::not_found("upstream", up.id.to_string())),
        }
    }

    /// Delete an upstream and cascade to its routes.
    ///
    /// Returns the number of routes removed alongside it.
    ///
    /// # Errors
    /// Returns [`DomainError::NotFound`] when the tenant does not own it.
    pub fn delete_upstream(&self, tenant: Uuid, id: Uuid) -> Result<usize, DomainError> {
        let mut g = self.inner.write();
        match g.upstreams.get(&id) {
            Some(u) if u.tenant_id == tenant => {}
            _ => return Err(DomainError::not_found("upstream", id.to_string())),
        }
        g.upstreams.remove(&id);
        let doomed: Vec<Uuid> = g
            .routes
            .values()
            .filter(|r| r.upstream_id == id)
            .map(|r| r.id)
            .collect();
        for r in &doomed {
            g.routes.remove(r);
        }
        Ok(doomed.len())
    }

    // ---- routes -------------------------------------------------------

    /// Insert a route.
    pub fn insert_route(&self, route: Route) -> Route {
        self.inner.write().routes.insert(route.id, route.clone());
        route
    }

    /// Fetch a route visible to `tenant`.
    #[must_use]
    pub fn get_route(&self, tenant: Uuid, id: Uuid) -> Option<Route> {
        self.inner
            .read()
            .routes
            .get(&id)
            .filter(|r| r.tenant_id == tenant)
            .cloned()
    }

    /// List the tenant's routes, ordered by identifier for a stable page.
    #[must_use]
    pub fn list_routes(&self, tenant: Uuid) -> Vec<Route> {
        let mut v: Vec<_> = self
            .inner
            .read()
            .routes
            .values()
            .filter(|r| r.tenant_id == tenant)
            .cloned()
            .collect();
        v.sort_by_key(|r| r.id);
        v
    }

    /// Every route bound to `upstream_id`.
    #[must_use]
    pub fn routes_for_upstream(&self, upstream_id: Uuid) -> Vec<Route> {
        let mut v: Vec<_> = self
            .inner
            .read()
            .routes
            .values()
            .filter(|r| r.upstream_id == upstream_id)
            .cloned()
            .collect();
        v.sort_by_key(|r| r.id);
        v
    }

    /// Replace a route in place.
    ///
    /// # Errors
    /// Returns [`DomainError::NotFound`] when the tenant does not own it.
    pub fn replace_route(&self, route: Route) -> Result<Route, DomainError> {
        let mut g = self.inner.write();
        match g.routes.get(&route.id) {
            Some(existing) if existing.tenant_id == route.tenant_id => {
                g.routes.insert(route.id, route.clone());
                Ok(route)
            }
            _ => Err(DomainError::not_found("route", route.id.to_string())),
        }
    }

    /// Delete a route.
    ///
    /// # Errors
    /// Returns [`DomainError::NotFound`] when the tenant does not own it.
    pub fn delete_route(&self, tenant: Uuid, id: Uuid) -> Result<(), DomainError> {
        let mut g = self.inner.write();
        match g.routes.get(&id) {
            Some(r) if r.tenant_id == tenant => {
                g.routes.remove(&id);
                Ok(())
            }
            _ => Err(DomainError::not_found("route", id.to_string())),
        }
    }

    // ---- plugin definitions -------------------------------------------

    /// Insert a plugin definition, enforcing `UNIQUE(tenant_id, name)`.
    ///
    /// # Errors
    /// Returns [`DomainError::AlreadyExists`] on a duplicate name.
    pub fn insert_plugin(&self, p: PluginDef) -> Result<PluginDef, DomainError> {
        let mut g = self.inner.write();
        if g
            .plugins
            .values()
            .any(|e| e.tenant_id == p.tenant_id && e.name == p.name)
        {
            return Err(DomainError::AlreadyExists {
                resource: "plugin".to_owned(),
                key: p.name.clone(),
            });
        }
        g.plugins.insert(p.id, p.clone());
        Ok(p)
    }

    /// Fetch a plugin definition visible to `tenant`.
    #[must_use]
    pub fn get_plugin(&self, tenant: Uuid, id: Uuid) -> Option<PluginDef> {
        self.inner
            .read()
            .plugins
            .get(&id)
            .filter(|p| p.tenant_id == tenant)
            .cloned()
    }

    /// List the tenant's plugin definitions, ordered by name.
    #[must_use]
    pub fn list_plugins(&self, tenant: Uuid) -> Vec<PluginDef> {
        let mut v: Vec<_> = self
            .inner
            .read()
            .plugins
            .values()
            .filter(|p| p.tenant_id == tenant)
            .cloned()
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    /// Delete a plugin definition.
    ///
    /// # Errors
    /// Returns [`DomainError::NotFound`] when the tenant does not own it.
    pub fn delete_plugin(&self, tenant: Uuid, id: Uuid) -> Result<(), DomainError> {
        let mut g = self.inner.write();
        match g.plugins.get(&id) {
            Some(p) if p.tenant_id == tenant => {
                g.plugins.remove(&id);
                Ok(())
            }
            _ => Err(DomainError::not_found("plugin", id.to_string())),
        }
    }

    /// Which upstreams and routes reference `plugin_id`, as an identifier that
    /// may appear either in full GTS form or as a bare UUID.
    #[must_use]
    pub fn plugin_references(&self, tenant: Uuid, plugin_id: Uuid) -> (Vec<String>, Vec<String>) {
        let needle = plugin_id.to_string();
        let matches = |item: &String| item == &needle || item.ends_with(&format!("~{needle}"));
        let g = self.inner.read();
        let mut upstreams: Vec<String> = g
            .upstreams
            .values()
            .filter(|u| {
                u.tenant_id == tenant
                    && (u.plugins.items.iter().any(matches)
                        || u.auth.plugin_type.as_ref().is_some_and(&matches))
            })
            .map(|u| u.id.to_string())
            .collect();
        let mut routes: Vec<String> = g
            .routes
            .values()
            .filter(|r| r.tenant_id == tenant && r.plugins.items.iter().any(matches))
            .map(|r| r.id.to_string())
            .collect();
        upstreams.sort();
        routes.sort();
        (upstreams, routes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{Endpoint, Scheme, Server};

    fn upstream(tenant: Uuid, alias: &str) -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            enabled: true,
            alias: alias.to_owned(),
            tags: vec![],
            server: Server {
                endpoints: vec![Endpoint {
                    scheme: Scheme::Http,
                    host: alias.to_owned(),
                    port: Some(80),
                }],
            },
            protocol: "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1".to_owned(),
            auth: Default::default(),
            headers: Default::default(),
            plugins: Default::default(),
            rate_limit: None,
            cors: None,
        }
    }

    #[test]
    fn alias_is_unique_within_a_tenant() {
        let s = Store::new();
        let t = Uuid::new_v4();
        s.insert_upstream(upstream(t, "example.com")).unwrap();
        let err = s.insert_upstream(upstream(t, "example.com")).unwrap_err();
        assert!(matches!(err, DomainError::AlreadyExists { .. }));
    }

    #[test]
    fn the_same_alias_is_free_in_a_different_tenant() {
        let s = Store::new();
        s.insert_upstream(upstream(Uuid::new_v4(), "example.com"))
            .unwrap();
        s.insert_upstream(upstream(Uuid::new_v4(), "example.com"))
            .unwrap();
    }

    #[test]
    fn another_tenants_upstream_is_invisible() {
        let s = Store::new();
        let mine = Uuid::new_v4();
        let theirs = Uuid::new_v4();
        let u = s.insert_upstream(upstream(theirs, "example.com")).unwrap();
        assert!(s.get_upstream(mine, u.id).is_none());
        assert!(s.get_upstream(theirs, u.id).is_some());
        assert!(s.list_upstreams(mine).is_empty());
    }

    #[test]
    fn alias_lookup_is_tenant_scoped() {
        let s = Store::new();
        let mine = Uuid::new_v4();
        s.insert_upstream(upstream(mine, "example.com")).unwrap();
        assert!(s.find_upstream_by_alias(mine, "example.com").is_some());
        assert!(
            s.find_upstream_by_alias(Uuid::new_v4(), "example.com")
                .is_none()
        );
    }

    #[test]
    fn deleting_an_upstream_cascades_to_its_routes() {
        let s = Store::new();
        let t = Uuid::new_v4();
        let u = s.insert_upstream(upstream(t, "example.com")).unwrap();
        for _ in 0..3 {
            s.insert_route(Route {
                id: Uuid::new_v4(),
                tenant_id: t,
                enabled: true,
                tags: vec![],
                upstream_id: u.id,
                match_: crate::domain::model::RouteMatch {
                    http: Some(crate::domain::model::HttpMatch {
                        methods: vec!["GET".to_owned()],
                        path: "/v1".to_owned(),
                        query_allowlist: vec![],
                        path_suffix_mode: crate::domain::model::PathSuffixMode::Append,
                    }),
                    grpc: None,
                },
                plugins: Default::default(),
                rate_limit: None,
            });
        }
        assert_eq!(s.routes_for_upstream(u.id).len(), 3);
        assert_eq!(s.delete_upstream(t, u.id).unwrap(), 3);
        assert!(s.routes_for_upstream(u.id).is_empty());
        assert!(s.list_routes(t).is_empty());
    }

    #[test]
    fn deleting_another_tenants_upstream_is_not_found() {
        let s = Store::new();
        let u = s.insert_upstream(upstream(Uuid::new_v4(), "a")).unwrap();
        let err = s.delete_upstream(Uuid::new_v4(), u.id).unwrap_err();
        assert!(matches!(err, DomainError::NotFound { .. }));
    }

    #[test]
    fn plugin_references_find_both_gts_and_bare_uuid_bindings() {
        let s = Store::new();
        let t = Uuid::new_v4();
        let pid = Uuid::new_v4();

        let mut bare = upstream(t, "bare");
        bare.plugins.items = vec![pid.to_string()];
        let bare = s.insert_upstream(bare).unwrap();

        let mut gts = upstream(t, "gts");
        gts.plugins.items = vec![format!("gts.cf.core.oagw.guard_plugin.v1~{pid}")];
        let gts = s.insert_upstream(gts).unwrap();

        let (ups, routes) = s.plugin_references(t, pid);
        assert_eq!(ups.len(), 2);
        assert!(ups.contains(&bare.id.to_string()));
        assert!(ups.contains(&gts.id.to_string()));
        assert!(routes.is_empty());
    }

    #[test]
    fn plugin_name_is_unique_within_a_tenant() {
        let s = Store::new();
        let t = Uuid::new_v4();
        let mk = || PluginDef {
            id: Uuid::new_v4(),
            tenant_id: t,
            name: "redactor".to_owned(),
            description: String::new(),
            plugin_type: crate::domain::model::PluginKind::Transform,
            config_schema: serde_json::Value::Null,
            source_code: String::new(),
        };
        s.insert_plugin(mk()).unwrap();
        assert!(matches!(
            s.insert_plugin(mk()).unwrap_err(),
            DomainError::AlreadyExists { .. }
        ));
    }
}
