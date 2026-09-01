//! In-memory control-plane store (`infra/storage`).
//!
//! The `oagw` gear carries no `database:` block (DESIGN §3.2 "Database
//! Schema" is honoured as the domain model, not as SQL migrations), so the
//! MVP control plane keeps its state in a single `RwLock`-protected map set.
//! A single lock is used on purpose: upstream→route cascade delete, route
//! match-uniqueness checks and the plugin in-use scan must observe one
//! consistent snapshot.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::RwLock;
use uuid::Uuid;

use crate::domain::error::{DomainError, ResourceKind};
use crate::domain::models::{Plugin, Route, Upstream};
use crate::domain::repo::ControlPlaneStore;

#[derive(Debug, Default)]
struct StoreState {
    upstreams: HashMap<Uuid, Upstream>,
    routes: HashMap<Uuid, Route>,
    plugins: HashMap<Uuid, Plugin>,
}

/// Thread-safe in-memory [`ControlPlaneStore`].
#[derive(Debug, Default)]
pub struct InMemoryStore {
    state: RwLock<StoreState>,
}

impl InMemoryStore {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Number of stored upstreams (observability helper).
    #[must_use]
    pub fn upstream_count(&self) -> usize {
        self.state.read().upstreams.len()
    }

    /// Number of stored routes (observability helper).
    #[must_use]
    pub fn route_count(&self) -> usize {
        self.state.read().routes.len()
    }

    /// Number of stored plugins (observability helper).
    #[must_use]
    pub fn plugin_count(&self) -> usize {
        self.state.read().plugins.len()
    }

    fn missing(resource: ResourceKind, id: &Uuid) -> DomainError {
        DomainError::not_found(resource, &id.to_string())
    }
}

fn duplicate_id(kind: ResourceKind, id: &Uuid) -> DomainError {
    DomainError::Conflict {
        detail: format!("{} id '{id}' is already in use", kind.as_str()),
        invalid_value: Some(id.to_string()),
    }
}

#[async_trait]
impl ControlPlaneStore for InMemoryStore {
    async fn insert_upstream(&self, upstream: Upstream) -> Result<(), DomainError> {
        let mut state = self.state.write();
        if state.upstreams.contains_key(&upstream.id) {
            return Err(duplicate_id(ResourceKind::Upstream, &upstream.id));
        }
        state.upstreams.insert(upstream.id, upstream);
        Ok(())
    }

    async fn get_upstream(
        &self,
        tenant_id: uuid::Uuid,
        id: uuid::Uuid,
    ) -> Result<Option<Upstream>, DomainError> {
        let state = self.state.read();
        Ok(state
            .upstreams
            .get(&id)
            .filter(|upstream| upstream.tenant_id == tenant_id)
            .cloned())
    }

    async fn find_upstream_by_alias(
        &self,
        tenant_id: uuid::Uuid,
        alias: &str,
    ) -> Result<Option<Upstream>, DomainError> {
        let state = self.state.read();
        Ok(state
            .upstreams
            .values()
            .find(|upstream| upstream.tenant_id == tenant_id && upstream.alias == alias)
            .cloned())
    }

    async fn list_upstreams(&self, tenant_id: uuid::Uuid) -> Result<Vec<Upstream>, DomainError> {
        let state = self.state.read();
        let mut rows: Vec<Upstream> = state
            .upstreams
            .values()
            .filter(|upstream| upstream.tenant_id == tenant_id)
            .cloned()
            .collect();
        drop(state);
        rows.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        Ok(rows)
    }

    async fn update_upstream(&self, upstream: Upstream) -> Result<(), DomainError> {
        let mut state = self.state.write();
        if !state.upstreams.contains_key(&upstream.id) {
            return Err(Self::missing(ResourceKind::Upstream, &upstream.id));
        }
        state.upstreams.insert(upstream.id, upstream);
        Ok(())
    }

    async fn delete_upstream(&self, tenant_id: uuid::Uuid, id: uuid::Uuid) -> Result<bool, DomainError> {
        let mut state = self.state.write();
        let exists = state
            .upstreams
            .get(&id)
            .is_some_and(|upstream| upstream.tenant_id == tenant_id);
        if exists {
            state.upstreams.remove(&id);
        }
        Ok(exists)
    }

    async fn insert_route(&self, route: Route) -> Result<(), DomainError> {
        let mut state = self.state.write();
        if state.routes.contains_key(&route.id) {
            return Err(duplicate_id(ResourceKind::Route, &route.id));
        }
        state.routes.insert(route.id, route);
        Ok(())
    }

    async fn get_route(&self, tenant_id: uuid::Uuid, id: uuid::Uuid) -> Result<Option<Route>, DomainError> {
        let state = self.state.read();
        Ok(state
            .routes
            .get(&id)
            .filter(|route| route.tenant_id == tenant_id)
            .cloned())
    }

    async fn list_routes(&self, tenant_id: uuid::Uuid) -> Result<Vec<Route>, DomainError> {
        let state = self.state.read();
        let mut rows: Vec<Route> = state
            .routes
            .values()
            .filter(|route| route.tenant_id == tenant_id)
            .cloned()
            .collect();
        drop(state);
        rows.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        Ok(rows)
    }

    async fn list_routes_by_upstream(
        &self,
        tenant_id: uuid::Uuid,
        upstream_id: uuid::Uuid,
    ) -> Result<Vec<Route>, DomainError> {
        let state = self.state.read();
        let mut rows: Vec<Route> = state
            .routes
            .values()
            .filter(|route| route.tenant_id == tenant_id && route.upstream_id == upstream_id)
            .cloned()
            .collect();
        drop(state);
        rows.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        Ok(rows)
    }

    async fn update_route(&self, route: Route) -> Result<(), DomainError> {
        let mut state = self.state.write();
        if !state.routes.contains_key(&route.id) {
            return Err(Self::missing(ResourceKind::Route, &route.id));
        }
        state.routes.insert(route.id, route);
        Ok(())
    }

    async fn delete_route(&self, tenant_id: uuid::Uuid, id: uuid::Uuid) -> Result<bool, DomainError> {
        let mut state = self.state.write();
        let exists = state
            .routes
            .get(&id)
            .is_some_and(|route| route.tenant_id == tenant_id);
        if exists {
            state.routes.remove(&id);
        }
        Ok(exists)
    }

    async fn insert_plugin(&self, plugin: Plugin) -> Result<(), DomainError> {
        let mut state = self.state.write();
        if state.plugins.contains_key(&plugin.id) {
            return Err(duplicate_id(ResourceKind::Plugin, &plugin.id));
        }
        state.plugins.insert(plugin.id, plugin);
        Ok(())
    }

    async fn get_plugin(&self, tenant_id: uuid::Uuid, id: uuid::Uuid) -> Result<Option<Plugin>, DomainError> {
        let state = self.state.read();
        Ok(state
            .plugins
            .get(&id)
            .filter(|plugin| plugin.tenant_id == tenant_id)
            .cloned())
    }

    async fn find_plugin_by_name(
        &self,
        tenant_id: uuid::Uuid,
        name: &str,
    ) -> Result<Option<Plugin>, DomainError> {
        let state = self.state.read();
        Ok(state
            .plugins
            .values()
            .find(|plugin| plugin.tenant_id == tenant_id && plugin.name == name)
            .cloned())
    }

    async fn list_plugins(&self, tenant_id: uuid::Uuid) -> Result<Vec<Plugin>, DomainError> {
        let state = self.state.read();
        let mut rows: Vec<Plugin> = state
            .plugins
            .values()
            .filter(|plugin| plugin.tenant_id == tenant_id)
            .cloned()
            .collect();
        drop(state);
        rows.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        Ok(rows)
    }

    async fn delete_plugin(&self, tenant_id: uuid::Uuid, id: uuid::Uuid) -> Result<bool, DomainError> {
        let mut state = self.state.write();
        let exists = state
            .plugins
            .get(&id)
            .is_some_and(|plugin| plugin.tenant_id == tenant_id);
        if exists {
            state.plugins.remove(&id);
        }
        Ok(exists)
    }
}
