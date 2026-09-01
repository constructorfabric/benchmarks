//! In-memory repositories behind the domain ports.
//!
//! Keys are `(tenant, id)` pairs so tenant isolation is structural: a row of
//! another tenant is not merely filtered out, it is unreachable, which is what
//! `DESIGN` §3.3 (Tenant Scoping) requires — an ancestor resource reads as a
//! `404`, not as an access violation.

use std::collections::BTreeMap;
use std::sync::Arc;

use parking_lot::RwLock;

use crate::domain::error::DomainError;
use crate::domain::model::{Plugin, PluginType, Route, Upstream};
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};

#[derive(Debug, Default)]
struct UpstreamTable {
    rows: BTreeMap<(uuid::Uuid, uuid::Uuid), Upstream>,
}

#[derive(Debug, Default)]
struct RouteTable {
    rows: Vec<Route>,
}

#[derive(Debug, Default)]
struct PluginTable {
    rows: Vec<Plugin>,
}

/// Process-wide, non-durable control-plane store.
#[derive(Debug, Default)]
pub struct MemoryStore {
    upstreams: RwLock<UpstreamTable>,
    routes: RwLock<RouteTable>,
    plugins: RwLock<PluginTable>,
}

impl MemoryStore {
    /// Create an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// [`UpstreamRepository`] over [`MemoryStore`].
#[derive(Clone)]
pub struct MemoryUpstreamRepository {
    store: Arc<MemoryStore>,
}

impl MemoryUpstreamRepository {
    /// Bind the repository to a store.
    #[must_use]
    pub fn new(store: Arc<MemoryStore>) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl UpstreamRepository for MemoryUpstreamRepository {
    async fn insert(&self, upstream: &Upstream) -> Result<(), DomainError> {
        let mut table = self.store.upstreams.write();
        let key = (upstream.tenant_id, upstream.id);
        if table.rows.contains_key(&key) {
            return Err(DomainError::Conflict {
                detail: format!("upstream {} already exists", upstream.id),
            });
        }
        if table.rows.values().any(|existing| {
            existing.tenant_id == upstream.tenant_id && existing.alias == upstream.alias
        }) {
            return Err(DomainError::Conflict {
                detail: format!("an upstream is already routed as '{}'", upstream.alias),
            });
        }
        table.rows.insert(key, upstream.clone());
        Ok(())
    }

    async fn update(&self, upstream: &Upstream) -> Result<(), DomainError> {
        let mut table = self.store.upstreams.write();
        let key = (upstream.tenant_id, upstream.id);
        if !table.rows.contains_key(&key) {
            return Err(DomainError::NotFound {
                resource: crate::domain::model::resource_gts_id(
                    crate::domain::model::UPSTREAM_TYPE,
                    upstream.id,
                ),
            });
        }
        table.rows.insert(key, upstream.clone());
        Ok(())
    }

    async fn find(
        &self,
        tenant: uuid::Uuid,
        id: uuid::Uuid,
    ) -> Result<Option<Upstream>, DomainError> {
        Ok(self.store.upstreams.read().rows.get(&(tenant, id)).cloned())
    }

    async fn find_by_alias(
        &self,
        tenant: uuid::Uuid,
        alias: &str,
    ) -> Result<Option<Upstream>, DomainError> {
        Ok(self
            .store
            .upstreams
            .read()
            .rows
            .values()
            .find(|existing| existing.tenant_id == tenant && existing.alias == alias)
            .cloned())
    }

    async fn list(&self, tenant: uuid::Uuid) -> Result<Vec<Upstream>, DomainError> {
        let mut rows: Vec<Upstream> = self
            .store
            .upstreams
            .read()
            .rows
            .values()
            .filter(|existing| existing.tenant_id == tenant)
            .cloned()
            .collect();
        rows.sort_by(|a, b| a.alias.cmp(&b.alias).then(a.id.cmp(&b.id)));
        Ok(rows)
    }

    async fn delete(&self, tenant: uuid::Uuid, id: uuid::Uuid) -> Result<bool, DomainError> {
        Ok(self
            .store
            .upstreams
            .write()
            .rows
            .remove(&(tenant, id))
            .is_some())
    }
}

/// [`RouteRepository`] over [`MemoryStore`].
#[derive(Clone)]
pub struct MemoryRouteRepository {
    store: Arc<MemoryStore>,
}

impl MemoryRouteRepository {
    /// Bind the repository to a store.
    #[must_use]
    pub fn new(store: Arc<MemoryStore>) -> Self {
        Self { store }
    }

    fn priority_order(a: &Route, b: &Route) -> std::cmp::Ordering {
        b.priority.cmp(&a.priority).then_with(|| a.id.cmp(&b.id))
    }
}

#[async_trait::async_trait]
impl RouteRepository for MemoryRouteRepository {
    async fn insert(&self, route: &Route) -> Result<(), DomainError> {
        let mut table = self.store.routes.write();
        if table.rows.iter().any(|existing| existing.id == route.id) {
            return Err(DomainError::Conflict {
                detail: format!("route {} already exists", route.id),
            });
        }
        table.rows.push(route.clone());
        Ok(())
    }

    async fn update(&self, route: &Route) -> Result<(), DomainError> {
        let mut table = self.store.routes.write();
        let Some(slot) = table
            .rows
            .iter_mut()
            .find(|existing| existing.id == route.id)
        else {
            return Err(DomainError::NotFound {
                resource: crate::domain::model::resource_gts_id(
                    crate::domain::model::ROUTE_TYPE,
                    route.id,
                ),
            });
        };
        *slot = route.clone();
        Ok(())
    }

    async fn find(&self, tenant: uuid::Uuid, id: uuid::Uuid) -> Result<Option<Route>, DomainError> {
        Ok(self
            .store
            .routes
            .read()
            .rows
            .iter()
            .find(|existing| existing.tenant_id == tenant && existing.id == id)
            .cloned())
    }

    async fn list(&self, tenant: uuid::Uuid) -> Result<Vec<Route>, DomainError> {
        let mut rows: Vec<Route> = self
            .store
            .routes
            .read()
            .rows
            .iter()
            .filter(|existing| existing.tenant_id == tenant)
            .cloned()
            .collect();
        rows.sort_by(|a, b| {
            a.upstream_id
                .cmp(&b.upstream_id)
                .then(Self::priority_order(a, b))
        });
        Ok(rows)
    }

    async fn list_by_upstream(
        &self,
        tenant: uuid::Uuid,
        upstream: uuid::Uuid,
    ) -> Result<Vec<Route>, DomainError> {
        let mut rows: Vec<Route> = self
            .store
            .routes
            .read()
            .rows
            .iter()
            .filter(|existing| existing.tenant_id == tenant && existing.upstream_id == upstream)
            .cloned()
            .collect();
        rows.sort_by(Self::priority_order);
        Ok(rows)
    }

    async fn delete(&self, tenant: uuid::Uuid, id: uuid::Uuid) -> Result<bool, DomainError> {
        let mut table = self.store.routes.write();
        let Some(index) = table
            .rows
            .iter()
            .position(|existing| existing.tenant_id == tenant && existing.id == id)
        else {
            return Ok(false);
        };
        table.rows.remove(index);
        Ok(true)
    }
}

/// [`PluginRepository`] over [`MemoryStore`].
#[derive(Clone)]
pub struct MemoryPluginRepository {
    store: Arc<MemoryStore>,
    upstreams: Arc<dyn UpstreamRepository>,
    routes: Arc<dyn RouteRepository>,
}

impl MemoryPluginRepository {
    /// Bind the repository to a store and the tables it must scan for
    /// "plugin in use" checks.
    #[must_use]
    pub fn new(
        store: Arc<MemoryStore>,
        upstreams: Arc<dyn UpstreamRepository>,
        routes: Arc<dyn RouteRepository>,
    ) -> Self {
        Self {
            store,
            upstreams,
            routes,
        }
    }
}

#[async_trait::async_trait]
impl PluginRepository for MemoryPluginRepository {
    async fn insert(&self, plugin: &Plugin) -> Result<(), DomainError> {
        let mut table = self.store.plugins.write();
        if table
            .rows
            .iter()
            .any(|existing| existing.tenant_id == plugin.tenant_id && existing.id == plugin.id)
        {
            return Err(DomainError::Conflict {
                detail: format!("plugin {} already exists", plugin.id),
            });
        }
        table.rows.push(plugin.clone());
        Ok(())
    }

    async fn find(
        &self,
        tenant: uuid::Uuid,
        id: uuid::Uuid,
    ) -> Result<Option<Plugin>, DomainError> {
        Ok(self
            .store
            .plugins
            .read()
            .rows
            .iter()
            .find(|existing| existing.tenant_id == tenant && existing.id == id)
            .cloned())
    }

    async fn list(
        &self,
        tenant: uuid::Uuid,
        kind: Option<PluginType>,
    ) -> Result<Vec<Plugin>, DomainError> {
        let mut rows: Vec<Plugin> = self
            .store
            .plugins
            .read()
            .rows
            .iter()
            .filter(|existing| {
                existing.tenant_id == tenant && kind.is_none_or(|k| existing.plugin_type == k)
            })
            .cloned()
            .collect();
        rows.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
        Ok(rows)
    }

    async fn references(
        &self,
        tenant: uuid::Uuid,
        id: uuid::Uuid,
    ) -> Result<(Vec<String>, Vec<String>), DomainError> {
        let mut upstream_refs: Vec<String> = Vec::new();
        for upstream in self.upstreams.list(tenant).await? {
            if references_plugin(upstream.auth.as_ref(), id)
                || chain_references(upstream.plugins.as_ref(), id)
            {
                upstream_refs.push(crate::domain::model::resource_gts_id(
                    crate::domain::model::UPSTREAM_TYPE,
                    upstream.id,
                ));
            }
        }
        let mut route_refs: Vec<String> = Vec::new();
        for route in self.routes.list(tenant).await? {
            if chain_references(route.plugins.as_ref(), id) {
                route_refs.push(crate::domain::model::resource_gts_id(
                    crate::domain::model::ROUTE_TYPE,
                    route.id,
                ));
            }
        }
        Ok((upstream_refs, route_refs))
    }

    async fn delete(&self, tenant: uuid::Uuid, id: uuid::Uuid) -> Result<bool, DomainError> {
        let mut table = self.store.plugins.write();
        let Some(index) = table
            .rows
            .iter()
            .position(|existing| existing.tenant_id == tenant && existing.id == id)
        else {
            return Ok(false);
        };
        table.rows.remove(index);
        Ok(true)
    }
}

/// `true` when an auth configuration names the plugin row.
fn references_plugin(auth: Option<&crate::domain::model::AuthConfig>, id: uuid::Uuid) -> bool {
    auth.is_some_and(|auth| {
        auth.plugin_type
            .as_deref()
            .is_some_and(|reference| crate::domain::plugin::uuid_tail(reference) == Some(id))
    })
}

/// `true` when a plugin chain binds the plugin id.
fn chain_references(chain: Option<&crate::domain::model::PluginsConfig>, id: uuid::Uuid) -> bool {
    chain.is_some_and(|chain| {
        chain
            .items
            .iter()
            .any(|item| crate::domain::plugin::uuid_tail(item.plugin_ref()) == Some(id))
    })
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "storage_tests.rs"]
mod tests;
