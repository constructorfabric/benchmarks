//! In-memory repositories (`dashmap` / `parking_lot` backed).
//!
//! There is no database in this implementation: the Control Plane keeps all
//! configuration in process memory (ADR-0006 in-memory state). Every
//! collection lives behind a `parking_lot::RwLock` so a write is atomic and
//! reads are short-lived; an alias index (`dashmap`) keeps the hot lookup
//! lock-free.

use std::sync::Arc;

use dashmap::DashMap;
use parking_lot::RwLock;

use crate::domain::error::DomainError;
use crate::domain::model::{Plugin, PluginType, Route, Upstream};
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};

/// All stored configuration, guarded as one unit.
#[derive(Default)]
struct Store {
    /// Upstreams ordered by creation.
    upstreams: Vec<Upstream>,
    /// Routes ordered by creation.
    routes: Vec<Route>,
    /// Tenant-defined plugins ordered by creation.
    plugins: Vec<Plugin>,
}

/// In-memory OAGW storage implementing all three repository traits.
#[derive(Clone, Default)]
pub struct InMemoryStore {
    state: Arc<RwLock<Store>>,
    /// `(tenant_id, alias) -> upstream_id` index.
    alias_index: Arc<DashMap<(uuid::Uuid, String), uuid::Uuid>>,
}

impl InMemoryStore {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Drops every stored configuration entry.
    pub fn clear(&self) {
        let mut state = self.state.write();
        state.upstreams.clear();
        state.routes.clear();
        state.plugins.clear();
        drop(state);
        self.alias_index.clear();
    }

    /// Number of stored upstreams (diagnostics only).
    #[must_use]
    pub fn upstream_count(&self) -> usize {
        self.state.read().upstreams.len()
    }

    /// Number of stored routes (diagnostics only).
    #[must_use]
    pub fn route_count(&self) -> usize {
        self.state.read().routes.len()
    }

    /// Number of stored plugins (diagnostics only).
    #[must_use]
    pub fn plugin_count(&self) -> usize {
        self.state.read().plugins.len()
    }

    /// Snapshot of all upstreams, used for hierarchy walks.
    #[must_use]
    pub fn all_upstreams(&self) -> Vec<Upstream> {
        self.state.read().upstreams.clone()
    }

    /// Snapshot of all routes, used for hierarchy walks.
    #[must_use]
    pub fn all_routes(&self) -> Vec<Route> {
        self.state.read().routes.clone()
    }

    /// Snapshot of all plugins, used for reference scans.
    #[must_use]
    pub fn all_plugins(&self) -> Vec<Plugin> {
        self.state.read().plugins.clone()
    }
}

impl UpstreamRepository for InMemoryStore {
    fn insert(&self, upstream: Upstream) -> Result<(), DomainError> {
        let alias = upstream.alias.clone();
        let tenant = upstream.tenant_id;
        let mut state = self.state.write();
        if state
            .upstreams
            .iter()
            .any(|u| u.tenant_id == tenant && u.alias == alias)
        {
            return Err(DomainError::Conflict(format!(
                "an upstream with alias '{alias}' already exists for this tenant"
            )));
        }
        self.alias_index
            .insert((tenant, alias.clone()), upstream.id);
        state.upstreams.push(upstream);
        Ok(())
    }

    fn get(&self, tenant_id: uuid::Uuid, id: uuid::Uuid) -> Option<Upstream> {
        self.state
            .read()
            .upstreams
            .iter()
            .find(|u| u.id == id && u.tenant_id == tenant_id)
            .cloned()
    }

    fn find_by_alias(&self, tenant_id: uuid::Uuid, alias: &str) -> Option<Upstream> {
        self.state
            .read()
            .upstreams
            .iter()
            .find(|u| u.tenant_id == tenant_id && u.alias.eq_ignore_ascii_case(alias))
            .cloned()
    }

    fn list(&self, tenant_id: uuid::Uuid) -> Vec<Upstream> {
        let mut owned: Vec<Upstream> = self
            .state
            .read()
            .upstreams
            .iter()
            .filter(|u| u.tenant_id == tenant_id)
            .cloned()
            .collect();
        owned.sort_by_key(|upstream| upstream.created_at);
        owned
    }

    fn replace(&self, upstream: Upstream) -> Result<(), DomainError> {
        let id = upstream.id;
        let tenant = upstream.tenant_id;
        let alias = upstream.alias.clone();
        let mut state = self.state.write();
        if state
            .upstreams
            .iter()
            .any(|u| u.tenant_id == tenant && u.id != id && u.alias == alias)
        {
            return Err(DomainError::Conflict(format!(
                "an upstream with alias '{alias}' already exists for this tenant"
            )));
        }
        match state
            .upstreams
            .iter_mut()
            .find(|u| u.id == id && u.tenant_id == tenant)
        {
            Some(slot) => {
                *slot = upstream;
                Ok(())
            }
            None => Err(DomainError::Validation("upstream not found".to_owned())),
        }
    }

    fn delete(&self, tenant_id: uuid::Uuid, id: uuid::Uuid) -> bool {
        let mut state = self.state.write();
        let before = state.upstreams.len();
        state
            .upstreams
            .retain(|u| !(u.id == id && u.tenant_id == tenant_id));
        let removed = state.upstreams.len() != before;
        drop(state);
        if removed {
            let stale: Vec<(uuid::Uuid, String)> = self
                .alias_index
                .iter()
                .filter(|entry| *entry.value() == id)
                .map(|entry| entry.key().clone())
                .collect();
            for key in stale {
                self.alias_index.remove(&key);
            }
        }
        removed
    }
}

impl RouteRepository for InMemoryStore {
    fn insert(&self, route: Route) -> Result<(), DomainError> {
        let key = route.match_key();
        let upstream = route.upstream_id;
        let id = route.id;
        let mut state = self.state.write();
        // The uniqueness check happens under the same write guard as the
        // insert, so two concurrent creates cannot both win the pre-check.
        if state.routes.iter().any(|existing| {
            existing.upstream_id == upstream
                && existing.id != id
                && existing.enabled
                && existing.match_key() == key
        }) {
            return Err(DomainError::Conflict(format!(
                "a route already matches {key}"
            )));
        }
        state.routes.push(route);
        Ok(())
    }

    fn get(&self, tenant_id: uuid::Uuid, id: uuid::Uuid) -> Option<Route> {
        self.state
            .read()
            .routes
            .iter()
            .find(|r| r.id == id && r.tenant_id == tenant_id)
            .cloned()
    }

    fn get_any_tenant(&self, id: uuid::Uuid) -> Option<Route> {
        self.state
            .read()
            .routes
            .iter()
            .find(|r| r.id == id)
            .cloned()
    }

    fn list(&self, tenant_id: uuid::Uuid) -> Vec<Route> {
        let mut owned: Vec<Route> = self
            .state
            .read()
            .routes
            .iter()
            .filter(|r| r.tenant_id == tenant_id)
            .cloned()
            .collect();
        owned.sort_by_key(|route| route.created_at);
        owned
    }

    fn list_by_upstream(&self, upstream_id: uuid::Uuid) -> Vec<Route> {
        self.state
            .read()
            .routes
            .iter()
            .filter(|r| r.upstream_id == upstream_id)
            .cloned()
            .collect()
    }

    fn replace(&self, route: Route) -> Result<(), DomainError> {
        let id = route.id;
        let tenant = route.tenant_id;
        let key = route.match_key();
        let upstream = route.upstream_id;
        let mut state = self.state.write();
        if state.routes.iter().any(|existing| {
            existing.upstream_id == upstream
                && existing.id != id
                && existing.enabled
                && existing.match_key() == key
        }) {
            return Err(DomainError::Conflict(format!(
                "a route already matches {key}"
            )));
        }
        match state
            .routes
            .iter_mut()
            .find(|r| r.id == id && r.tenant_id == tenant)
        {
            Some(slot) => {
                *slot = route;
                Ok(())
            }
            None => Err(DomainError::Validation("route not found".to_owned())),
        }
    }

    fn delete(&self, tenant_id: uuid::Uuid, id: uuid::Uuid) -> bool {
        let mut state = self.state.write();
        let before = state.routes.len();
        state
            .routes
            .retain(|r| !(r.id == id && r.tenant_id == tenant_id));
        state.routes.len() != before
    }
}

impl PluginRepository for InMemoryStore {
    fn insert(&self, plugin: Plugin) -> Result<(), DomainError> {
        let name = plugin.name.clone();
        let tenant = plugin.tenant_id;
        let mut state = self.state.write();
        if state
            .plugins
            .iter()
            .any(|p| p.tenant_id == tenant && p.name == name)
        {
            return Err(DomainError::Conflict(format!(
                "a plugin named '{name}' already exists for this tenant"
            )));
        }
        state.plugins.push(plugin);
        Ok(())
    }

    fn get(&self, id: uuid::Uuid) -> Option<Plugin> {
        self.state
            .read()
            .plugins
            .iter()
            .find(|p| p.id == id)
            .cloned()
    }

    fn list(&self, tenant_id: uuid::Uuid, plugin_type: Option<PluginType>) -> Vec<Plugin> {
        let mut owned: Vec<Plugin> = self
            .state
            .read()
            .plugins
            .iter()
            .filter(|p| p.tenant_id == tenant_id && plugin_type.is_none_or(|t| p.plugin_type == t))
            .cloned()
            .collect();
        owned.sort_by_key(|plugin| plugin.created_at);
        owned
    }

    fn delete(&self, id: uuid::Uuid) -> bool {
        let mut state = self.state.write();
        let before = state.plugins.len();
        state.plugins.retain(|p| p.id != id);
        state.plugins.len() != before
    }

    fn find_by_name(&self, tenant_id: uuid::Uuid, name: &str) -> Option<Plugin> {
        self.state
            .read()
            .plugins
            .iter()
            .find(|p| p.tenant_id == tenant_id && p.name == name)
            .cloned()
    }
}

/// Typed facade over the repository traits.
///
/// The three repository traits declare overlapping method names (`insert`,
/// `get`, `list`, `delete`), so calling them through a concrete
/// [`InMemoryStore`] is ambiguous. The Control Plane goes through these
/// inherent wrappers instead of importing all three traits.
impl InMemoryStore {
    /// Inserts an upstream.
    pub fn put_upstream(&self, upstream: Upstream) -> Result<(), DomainError> {
        UpstreamRepository::insert(self, upstream)
    }

    /// Replaces an upstream.
    pub fn save_upstream(&self, upstream: Upstream) -> Result<(), DomainError> {
        UpstreamRepository::replace(self, upstream)
    }

    /// Fetches an upstream owned by `tenant_id`.
    #[must_use]
    pub fn upstream(&self, tenant_id: uuid::Uuid, id: uuid::Uuid) -> Option<Upstream> {
        UpstreamRepository::get(self, tenant_id, id)
    }

    /// Fetches an upstream owned by `tenant_id` by alias.
    #[must_use]
    pub fn upstream_by_alias(&self, tenant_id: uuid::Uuid, alias: &str) -> Option<Upstream> {
        UpstreamRepository::find_by_alias(self, tenant_id, alias)
    }

    /// Lists the upstreams owned by `tenant_id`.
    #[must_use]
    pub fn upstreams(&self, tenant_id: uuid::Uuid) -> Vec<Upstream> {
        UpstreamRepository::list(self, tenant_id)
    }

    /// Deletes an upstream owned by `tenant_id`.
    pub fn remove_upstream(&self, tenant_id: uuid::Uuid, id: uuid::Uuid) -> bool {
        UpstreamRepository::delete(self, tenant_id, id)
    }

    /// Inserts a route.
    pub fn put_route(&self, route: Route) -> Result<(), DomainError> {
        RouteRepository::insert(self, route)
    }

    /// Replaces a route.
    pub fn save_route(&self, route: Route) -> Result<(), DomainError> {
        RouteRepository::replace(self, route)
    }

    /// Fetches a route owned by `tenant_id`.
    #[must_use]
    pub fn route(&self, tenant_id: uuid::Uuid, id: uuid::Uuid) -> Option<Route> {
        RouteRepository::get(self, tenant_id, id)
    }

    /// Fetches a route regardless of the owning tenant (proxy resolution).
    #[must_use]
    pub fn route_any_tenant(&self, id: uuid::Uuid) -> Option<Route> {
        RouteRepository::get_any_tenant(self, id)
    }

    /// Lists the routes owned by `tenant_id`.
    #[must_use]
    pub fn routes(&self, tenant_id: uuid::Uuid) -> Vec<Route> {
        RouteRepository::list(self, tenant_id)
    }

    /// Lists the routes bound to an upstream.
    #[must_use]
    pub fn routes_of_upstream(&self, upstream_id: uuid::Uuid) -> Vec<Route> {
        RouteRepository::list_by_upstream(self, upstream_id)
    }

    /// Deletes a route owned by `tenant_id`.
    pub fn remove_route(&self, tenant_id: uuid::Uuid, id: uuid::Uuid) -> bool {
        RouteRepository::delete(self, tenant_id, id)
    }

    /// Inserts a plugin.
    pub fn put_plugin(&self, plugin: Plugin) -> Result<(), DomainError> {
        PluginRepository::insert(self, plugin)
    }

    /// Fetches a plugin by id, regardless of the owning tenant.
    #[must_use]
    pub fn plugin(&self, id: uuid::Uuid) -> Option<Plugin> {
        PluginRepository::get(self, id)
    }

    /// Lists the plugins owned by `tenant_id`, optionally filtered by type.
    #[must_use]
    pub fn plugins(&self, tenant_id: uuid::Uuid, plugin_type: Option<PluginType>) -> Vec<Plugin> {
        PluginRepository::list(self, tenant_id, plugin_type)
    }

    /// Fetches a plugin owned by `tenant_id` by name.
    #[must_use]
    pub fn plugin_by_name(&self, tenant_id: uuid::Uuid, name: &str) -> Option<Plugin> {
        PluginRepository::find_by_name(self, tenant_id, name)
    }

    /// Deletes a plugin by id, regardless of the owning tenant.
    pub fn remove_plugin(&self, id: uuid::Uuid) -> bool {
        PluginRepository::delete(self, id)
    }
}

#[cfg(test)]
#[path = "storage_tests.rs"]
mod tests;
