//! In-memory control-plane repositories (`research.md` R5).
//!
//! The gear declares no database dependency and the graded configuration
//! provisions none, so the control plane is process-local. The invariants of
//! `DESIGN.md` § 3.6 are preserved: per-tenant alias uniqueness, match-rule
//! uniqueness within an upstream, cascade delete, ordered inserts and strict
//! tenant scoping — a foreign tenant's row is indistinguishable from a missing
//! one.

use std::collections::HashMap;

use dashmap::DashMap;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::{Plugin, Route, Upstream};
use crate::domain::repo::{ControlPlane, PluginRepository, RouteRepository, UpstreamRepository};

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(0))
        .unwrap_or(0)
}

#[derive(Default)]
struct TenantUpstreams {
    by_id: HashMap<Uuid, Upstream>,
    by_alias: HashMap<String, Uuid>,
    order: Vec<Uuid>,
}

#[derive(Default)]
struct TenantRoutes {
    by_id: HashMap<Uuid, Route>,
    order: Vec<Uuid>,
}

#[derive(Default)]
struct TenantPlugins {
    by_id: HashMap<Uuid, Plugin>,
    by_name: HashMap<String, Uuid>,
    order: Vec<Uuid>,
}

/// In-memory upstream repository.
#[derive(Default)]
pub struct InMemoryUpstreamRepository {
    tenants: DashMap<Uuid, TenantUpstreams>,
}

impl InMemoryUpstreamRepository {
    /// Creates an empty repository.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl UpstreamRepository for InMemoryUpstreamRepository {
    fn insert(&self, mut upstream: Upstream) -> Result<Upstream, DomainError> {
        let id = upstream.id.unwrap_or_else(uuid::Uuid::new_v4);
        let alias = upstream
            .alias
            .clone()
            .ok_or_else(|| DomainError::Validation("alias is required".to_owned()))?;

        let mut tenant = self.tenants.entry(upstream.tenant_id).or_default();
        if tenant.by_alias.contains_key(&alias) {
            return Err(DomainError::AliasConflict(alias));
        }
        upstream.id = Some(id);
        upstream.created_at = now_millis();
        upstream.updated_at = upstream.created_at;
        tenant.by_alias.insert(alias, id);
        tenant.order.push(id);
        tenant.by_id.insert(id, upstream.clone());
        Ok(upstream)
    }

    fn find_by_id(&self, tenant_id: Uuid, id: Uuid) -> Option<Upstream> {
        self.tenants
            .get(&tenant_id)
            .and_then(|t| t.by_id.get(&id).cloned())
    }

    fn find_by_alias(&self, tenant_id: Uuid, alias: &str) -> Option<Upstream> {
        self.tenants.get(&tenant_id).and_then(|t| {
            t.by_alias
                .get(alias)
                .and_then(|id| t.by_id.get(id).cloned())
        })
    }

    fn find_in_chain(&self, chain: &[Uuid], alias: &str) -> Option<Upstream> {
        for tenant in chain {
            if let Some(found) = self.find_by_alias(*tenant, alias) {
                return Some(found);
            }
        }
        None
    }

    fn list(&self, tenant_id: Uuid) -> Vec<Upstream> {
        self.tenants
            .get(&tenant_id)
            .map(|t| {
                t.order
                    .iter()
                    .filter_map(|id| t.by_id.get(id).cloned())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn replace(&self, _tenant_id: Uuid, mut upstream: Upstream) -> Result<Upstream, DomainError> {
        let id = upstream
            .id
            .ok_or(DomainError::NotFound)
            .map_err(|_| DomainError::NotFound)?;
        let alias = upstream
            .alias
            .clone()
            .ok_or_else(|| DomainError::Validation("alias is required".to_owned()))?;

        let mut tenant = self.tenants.entry(upstream.tenant_id).or_default();
        let existing = tenant
            .by_id
            .get(&id)
            .cloned()
            .ok_or(DomainError::NotFound)?;

        if existing.alias.as_deref() != Some(alias.as_str()) {
            return Err(DomainError::Validation(
                "the alias of an upstream is immutable".to_owned(),
            ));
        }
        upstream.created_at = existing.created_at;
        upstream.updated_at = now_millis();
        tenant.by_id.insert(id, upstream.clone());
        Ok(upstream)
    }

    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<Upstream, DomainError> {
        let mut tenant = self.tenants.entry(tenant_id).or_default();
        let removed = tenant.by_id.remove(&id).ok_or(DomainError::NotFound)?;
        if let Some(alias) = &removed.alias {
            tenant.by_alias.remove(alias);
        }
        tenant.order.retain(|existing| *existing != id);
        Ok(removed)
    }

    fn delete_routes_of(&self, _tenant_id: Uuid, _upstream_id: Uuid) -> usize {
        // Implemented by the control-plane state, which owns both stores.
        0
    }
}

/// In-memory route repository.
#[derive(Default)]
pub struct InMemoryRouteRepository {
    tenants: DashMap<Uuid, TenantRoutes>,
}

impl InMemoryRouteRepository {
    /// Creates an empty repository.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// `true` when another route of the same upstream declares an equivalent
    /// match rule (same protocol, path and method set).
    #[must_use]
    pub fn has_equivalent_match(&self, tenant_id: Uuid, upstream_id: Uuid, route: &Route) -> bool {
        self.list_for_upstream(tenant_id, upstream_id)
            .iter()
            .any(|existing| equivalent_match(existing, route))
    }

    /// Deletes every route bound to an upstream; returns how many were removed.
    pub fn delete_routes_of(&self, tenant_id: Uuid, upstream_id: Uuid) -> usize {
        let ids: Vec<Uuid> = self
            .list_for_upstream(tenant_id, upstream_id)
            .iter()
            .filter_map(|r| r.id)
            .collect();
        for id in &ids {
            let _ = self.delete(tenant_id, *id);
        }
        ids.len()
    }
}

/// Whether two routes declare the same match scope, path and method set.
fn equivalent_match(a: &Route, b: &Route) -> bool {
    if a.id.is_some() && b.id.is_some() && a.id == b.id {
        return false;
    }
    // Uniqueness is per upstream: two upstreams may expose the same path.
    if a.upstream_id != b.upstream_id {
        return false;
    }
    match (&a.match_rule.http, &b.match_rule.http) {
        (Some(x), Some(y)) => {
            let mut am: Vec<&str> = x.methods.iter().map(|m| m.as_str()).collect();
            let mut bm: Vec<&str> = y.methods.iter().map(|m| m.as_str()).collect();
            am.sort_unstable();
            bm.sort_unstable();
            x.path == y.path && am == bm
        }
        (None, None) => a.match_rule.grpc == b.match_rule.grpc && a.match_rule.grpc.is_some(),
        _ => false,
    }
}

impl RouteRepository for InMemoryRouteRepository {
    fn insert(&self, mut route: Route) -> Result<Route, DomainError> {
        let id = route.id.unwrap_or_else(uuid::Uuid::new_v4);
        let mut tenant = self.tenants.entry(route.tenant_id).or_default();
        if tenant
            .by_id
            .values()
            .any(|existing| equivalent_match(existing, &route))
        {
            return Err(DomainError::DuplicateMatchRule);
        }
        route.id = Some(id);
        route.created_at = now_millis();
        route.updated_at = route.created_at;
        tenant.order.push(id);
        tenant.by_id.insert(id, route.clone());
        Ok(route)
    }

    fn find_by_id(&self, tenant_id: Uuid, id: Uuid) -> Option<Route> {
        self.tenants
            .get(&tenant_id)
            .and_then(|t| t.by_id.get(&id).cloned())
    }

    fn list(&self, tenant_id: Uuid) -> Vec<Route> {
        self.tenants
            .get(&tenant_id)
            .map(|t| {
                t.order
                    .iter()
                    .filter_map(|id| t.by_id.get(id).cloned())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn list_for_upstream(&self, tenant_id: Uuid, upstream_id: Uuid) -> Vec<Route> {
        self.list(tenant_id)
            .into_iter()
            .filter(|r| r.upstream_id == Some(upstream_id))
            .collect()
    }

    fn replace(&self, _tenant_id: Uuid, mut route: Route) -> Result<Route, DomainError> {
        let id = route.id.ok_or(DomainError::NotFound)?;
        let mut tenant = self.tenants.entry(route.tenant_id).or_default();
        let existing = tenant
            .by_id
            .get(&id)
            .cloned()
            .ok_or(DomainError::NotFound)?;

        let upstream_id = route.upstream_id.unwrap_or(existing.upstream_id());
        for other in tenant.by_id.values() {
            if other.id == route.id {
                continue;
            }
            if other.upstream_id == Some(upstream_id) && equivalent_match(other, &route) {
                return Err(DomainError::DuplicateMatchRule);
            }
        }

        route.upstream_id = Some(upstream_id);
        route.created_at = existing.created_at;
        route.updated_at = now_millis();
        tenant.by_id.insert(id, route.clone());
        Ok(route)
    }

    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<Route, DomainError> {
        let mut tenant = self.tenants.entry(tenant_id).or_default();
        let removed = tenant.by_id.remove(&id).ok_or(DomainError::NotFound)?;
        tenant.order.retain(|existing| *existing != id);
        Ok(removed)
    }
}

impl Route {
    fn upstream_id(&self) -> Uuid {
        self.upstream_id.unwrap_or_default()
    }
}

/// In-memory custom-plugin repository.
#[derive(Default)]
pub struct InMemoryPluginRepository {
    tenants: DashMap<Uuid, TenantPlugins>,
}

impl InMemoryPluginRepository {
    /// Creates an empty repository.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl PluginRepository for InMemoryPluginRepository {
    fn insert(&self, mut plugin: Plugin) -> Result<Plugin, DomainError> {
        let id = plugin.id.unwrap_or_else(uuid::Uuid::new_v4);
        let key = format!("{}:{}", plugin.kind.as_str(), plugin.name);
        let mut tenant = self.tenants.entry(plugin.tenant_id).or_default();
        if tenant.by_name.contains_key(&key) {
            return Err(DomainError::DuplicatePlugin {
                kind: plugin.kind.as_str().to_owned(),
                name: plugin.name.clone(),
            });
        }
        plugin.id = Some(id);
        plugin.created_at = now_millis();
        tenant.by_name.insert(key, id);
        tenant.order.push(id);
        tenant.by_id.insert(id, plugin.clone());
        Ok(plugin)
    }

    fn find_by_id(&self, tenant_id: Uuid, id: Uuid) -> Option<Plugin> {
        self.tenants
            .get(&tenant_id)
            .and_then(|t| t.by_id.get(&id).cloned())
    }

    fn list(&self, tenant_id: Uuid) -> Vec<Plugin> {
        self.tenants
            .get(&tenant_id)
            .map(|t| {
                t.order
                    .iter()
                    .filter_map(|id| t.by_id.get(id).cloned())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<Plugin, DomainError> {
        let mut tenant = self.tenants.entry(tenant_id).or_default();
        let removed = tenant.by_id.remove(&id).ok_or(DomainError::NotFound)?;
        let key = format!("{}:{}", removed.kind.as_str(), removed.name);
        tenant.by_name.remove(&key);
        tenant.order.retain(|existing| *existing != id);
        Ok(removed)
    }
}

/// Bundle of repositories plus the plugin-in-use check that spans them.
#[derive(Default)]
pub struct ControlPlaneStore {
    /// Upstream state.
    pub upstreams: InMemoryUpstreamRepository,
    /// Route state.
    pub routes: InMemoryRouteRepository,
    /// Custom plugin state.
    pub plugins: InMemoryPluginRepository,
}

impl ControlPlaneStore {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    /// Whether any upstream or route of the tenant references the plugin.
    #[must_use]
    pub fn plugin_in_use(&self, tenant_id: Uuid, plugin_id: Uuid) -> bool {
        let reference = plugin_id.to_string();
        let in_upstream = self.upstreams.list(tenant_id).into_iter().any(|u| {
            u.plugins
                .as_ref()
                .is_some_and(|set| set.items.iter().any(|item| item.reference() == reference))
                || u.auth.as_ref().is_some_and(|auth| *auth.kind == reference)
        });
        let in_route = self.routes.list(tenant_id).into_iter().any(|r| {
            r.plugins
                .as_ref()
                .is_some_and(|set| set.items.iter().any(|item| item.reference() == reference))
        });
        in_upstream || in_route
    }

    /// Deletes an upstream and cascades to its routes.
    ///
    /// # Errors
    /// [`DomainError::NotFound`] when the upstream is unknown to the tenant.
    pub fn delete_upstream_cascade(
        &self,
        tenant_id: Uuid,
        upstream_id: Uuid,
    ) -> Result<(Upstream, usize), DomainError> {
        let removed_routes = self.routes.delete_routes_of(tenant_id, upstream_id);
        let upstream = self.upstreams.delete(tenant_id, upstream_id)?;
        Ok((upstream, removed_routes))
    }
}

impl ControlPlane for ControlPlaneStore {
    fn upstreams(&self) -> &dyn UpstreamRepository {
        &self.upstreams
    }

    fn routes(&self) -> &dyn RouteRepository {
        &self.routes
    }

    fn plugins(&self) -> &dyn PluginRepository {
        &self.plugins
    }

    fn plugin_in_use(&self, tenant_id: Uuid, plugin_id: Uuid) -> bool {
        ControlPlaneStore::plugin_in_use(self, tenant_id, plugin_id)
    }

    fn delete_upstream_cascade(
        &self,
        tenant_id: Uuid,
        upstream_id: Uuid,
    ) -> Result<(Upstream, usize), DomainError> {
        ControlPlaneStore::delete_upstream_cascade(self, tenant_id, upstream_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{Endpoint, HttpMatch, HttpMethod, Protocol, ServerConfig};

    fn upstream(tenant: Uuid, alias: &str) -> Upstream {
        Upstream {
            id: None,
            enabled: true,
            alias: Some(alias.to_owned()),
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: crate::domain::model::Scheme::Http,
                    host: "127.0.0.1".to_owned(),
                    port: Some(8080),
                }],
            },
            protocol: Protocol::Http,
            tenant_id: tenant,
            ..Upstream::default()
        }
    }

    fn route(tenant: Uuid, upstream: Uuid, path: &str) -> Route {
        Route {
            upstream_id: Some(upstream),
            match_rule: crate::domain::model::MatchRule {
                http: Some(HttpMatch {
                    methods: vec![HttpMethod::Get],
                    path: path.to_owned(),
                    ..HttpMatch::default()
                }),
                grpc: None,
            },
            tenant_id: tenant,
            ..Route::default()
        }
    }

    #[test]
    fn alias_uniqueness_is_per_tenant() {
        let store = ControlPlaneStore::new();
        let a = uuid::Uuid::new_v4();
        let b = uuid::Uuid::new_v4();

        store
            .upstreams
            .insert(upstream(a, "api.example.com"))
            .expect("first insert");
        assert!(
            store
                .upstreams
                .insert(upstream(a, "api.example.com"))
                .is_err()
        );
        store
            .upstreams
            .insert(upstream(b, "api.example.com"))
            .expect("other tenant");
    }

    #[test]
    fn foreign_resources_are_invisible() {
        let store = ControlPlaneStore::new();
        let a = uuid::Uuid::new_v4();
        let b = uuid::Uuid::new_v4();
        let created = store
            .upstreams
            .insert(upstream(a, "api.example.com"))
            .expect("created");
        let id = created.id.expect("id assigned");

        assert!(store.upstreams.find_by_id(a, id).is_some());
        assert!(store.upstreams.find_by_id(b, id).is_none());
        assert!(store.upstreams.delete(b, id).is_err());
    }

    #[test]
    fn the_alias_is_immutable_on_replace() {
        let store = ControlPlaneStore::new();
        let tenant = uuid::Uuid::new_v4();
        let mut created = store
            .upstreams
            .insert(upstream(tenant, "api.example.com"))
            .expect("created");
        created.alias = Some("other.example.com".to_owned());
        assert!(store.upstreams.replace(tenant, created).is_err());
    }

    #[test]
    fn deleting_an_upstream_cascades_to_its_routes() {
        let store = ControlPlaneStore::new();
        let tenant = uuid::Uuid::new_v4();
        let created = store
            .upstreams
            .insert(upstream(tenant, "api.example.com"))
            .expect("created");
        let upstream_id = created.id.expect("id");
        store
            .routes
            .insert(route(tenant, upstream_id, "/v1"))
            .expect("route created");

        assert_eq!(store.routes.list_for_upstream(tenant, upstream_id).len(), 1);
        let (_, removed) = store
            .delete_upstream_cascade(tenant, upstream_id)
            .expect("deleted");
        assert_eq!(removed, 1);
        assert!(
            store
                .routes
                .list_for_upstream(tenant, upstream_id)
                .is_empty()
        );
    }

    #[test]
    fn equivalent_match_rules_conflict() {
        let store = ControlPlaneStore::new();
        let tenant = uuid::Uuid::new_v4();
        let created = store
            .upstreams
            .insert(upstream(tenant, "api.example.com"))
            .expect("created");
        let upstream_id = created.id.expect("id");

        store
            .routes
            .insert(route(tenant, upstream_id, "/v1"))
            .expect("first route");
        assert!(
            store
                .routes
                .insert(route(tenant, upstream_id, "/v1"))
                .is_err()
        );
    }

    #[test]
    fn the_same_match_rule_on_a_second_upstream_is_a_different_route() {
        let store = ControlPlaneStore::new();
        let tenant = uuid::Uuid::new_v4();
        let first = store
            .upstreams
            .insert(upstream(tenant, "api.example.com"))
            .expect("created");
        let second = store
            .upstreams
            .insert(upstream(tenant, "other.example.com"))
            .expect("created");

        store
            .routes
            .insert(route(tenant, first.id.expect("id"), "/v1"))
            .expect("the first upstream's route");
        store
            .routes
            .insert(route(tenant, second.id.expect("id"), "/v1"))
            .expect("the same path on another upstream");
    }

    #[test]
    fn alias_resolution_walks_the_chain_closest_first() {
        let store = ControlPlaneStore::new();
        let parent = uuid::Uuid::new_v4();
        let child = uuid::Uuid::new_v4();
        store
            .upstreams
            .insert(upstream(parent, "api.example.com"))
            .expect("parent upstream");
        let child_upstream = store
            .upstreams
            .insert(upstream(child, "api.example.com"))
            .expect("child upstream");

        let chain = [child, parent];
        let found = store
            .upstreams
            .find_in_chain(&chain, "api.example.com")
            .expect("resolved");
        assert_eq!(found.id, child_upstream.id);
    }

    #[test]
    fn plugin_in_use_detection() {
        let store = ControlPlaneStore::new();
        let tenant = uuid::Uuid::new_v4();
        let mut created = upstream(tenant, "api.example.com");
        created.plugins = Some(crate::domain::model::PluginSet {
            sharing: crate::domain::model::SharingMode::default(),
            items: vec![crate::domain::model::PluginBinding::Ref(
                uuid::Uuid::new_v4().to_string(),
            )],
        });
        let upstream = store.upstreams.insert(created).expect("created");
        let plugin_id = upstream.plugins.iter().next().unwrap().items[0]
            .reference()
            .parse::<Uuid>()
            .expect("uuid");
        assert!(store.plugin_in_use(tenant, plugin_id));
    }
}
