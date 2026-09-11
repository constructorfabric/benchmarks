//! In-memory repository implementation.
//!
//! The gear crate's declared dependency set has no database driver, so the
//! Control Plane keeps its state in process, guarded by `parking_lot` locks.
//! The repositories implement the domain traits, so a durable implementation
//! can replace this one without touching domain or transport code.

use crate::domain::error::DomainError;
use crate::domain::model::{Plugin, Route, Upstream};
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};
use async_trait::async_trait;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

type Store<T> = Arc<RwLock<HashMap<(Uuid, String), T>>>;

fn conflict(message: String) -> DomainError {
    DomainError::Conflict(message)
}

fn not_found(kind: &str, key: &str) -> DomainError {
    DomainError::NotFound {
        kind: kind.to_owned(),
        target: key.to_owned(),
    }
}

/// In-memory [`UpstreamRepository`].
#[derive(Clone, Default)]
pub struct MemoryUpstreamRepo {
    store: Store<Upstream>,
}

impl MemoryUpstreamRepo {
    /// Creates an empty repository.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every stored upstream across tenants (used by the data plane).
    #[must_use]
    pub fn all(&self) -> Vec<Upstream> {
        self.store.read().values().cloned().collect()
    }
}

#[async_trait]
impl UpstreamRepository for MemoryUpstreamRepo {
    async fn create(&self, upstream: Upstream) -> Result<Upstream, DomainError> {
        let key = (upstream.tenant_id, upstream.alias.clone());
        let mut guard = self.store.write();
        if guard.contains_key(&key) {
            return Err(conflict(format!(
                "alias '{}' already exists",
                upstream.alias
            )));
        }
        guard.insert(key, upstream.clone());
        Ok(upstream)
    }

    async fn get(&self, tenant_id: Uuid, id: &str) -> Result<Upstream, DomainError> {
        self.store
            .read()
            .values()
            .find(|u| u.tenant_id == tenant_id && u.id == id)
            .cloned()
            .ok_or_else(|| not_found("upstream", id))
    }

    async fn find_by_alias(&self, tenant_id: Uuid, alias: &str) -> Result<Upstream, DomainError> {
        self.store
            .read()
            .get(&(tenant_id, alias.to_owned()))
            .cloned()
            .ok_or_else(|| not_found("upstream", alias))
    }

    async fn list(&self, tenant_id: Uuid) -> Result<Vec<Upstream>, DomainError> {
        let mut items: Vec<Upstream> = self
            .store
            .read()
            .iter()
            .filter(|((tenant, _), _)| *tenant == tenant_id)
            .map(|(_, v)| v.clone())
            .collect();
        items.sort_by(|a, b| a.alias.cmp(&b.alias));
        Ok(items)
    }

    async fn replace(&self, upstream: Upstream) -> Result<Upstream, DomainError> {
        let key = (upstream.tenant_id, upstream.alias.clone());
        let mut guard = self.store.write();
        match guard.get_mut(&key) {
            Some(existing) => {
                *existing = upstream.clone();
                Ok(upstream)
            }
            None => Err(not_found("upstream", &upstream.id)),
        }
    }

    async fn delete(&self, tenant_id: Uuid, id: &str) -> Result<(), DomainError> {
        let found = {
            let guard = self.store.read();
            guard
                .iter()
                .find(|((tenant, _), upstream)| *tenant == tenant_id && upstream.id == id)
                .map(|(key, _)| key.clone())
        };
        match found {
            Some(key) => {
                self.store.write().remove(&key);
                Ok(())
            }
            None => Err(not_found("upstream", id)),
        }
    }
}

/// In-memory [`RouteRepository`].
#[derive(Clone, Default)]
pub struct MemoryRouteRepo {
    store: Arc<RwLock<HashMap<String, Route>>>,
}

impl MemoryRouteRepo {
    /// Creates an empty repository.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every stored route across tenants (used by the data plane).
    #[must_use]
    pub fn all(&self) -> Vec<Route> {
        self.store.read().values().cloned().collect()
    }

    fn key(route: &Route) -> String {
        let methods = route
            .http_match()
            .map(|m| {
                m.methods
                    .iter()
                    .map(|method| method.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default();
        let path = route
            .http_match()
            .map(|m| m.path.clone())
            .unwrap_or_default();
        format!(
            "{}|{}|{}|{}",
            route.tenant_id, route.upstream_id, path, methods
        )
    }

    fn duplicate_exists(&self, route: &Route, skip_id: Option<&str>) -> bool {
        Self::duplicates(&self.store.read(), route, skip_id)
    }

    /// Whether another route matches the same rule, over a map the caller is
    /// already holding a lock on. The lock is not taken here so `replace` can
    /// run its check under the write guard it already holds.
    fn duplicates(store: &HashMap<String, Route>, route: &Route, skip_id: Option<&str>) -> bool {
        let key = Self::key(route);
        store.values().any(|existing| {
            existing.id != route.id
                && Some(existing.id.as_str()) != skip_id
                && Self::key(existing) == key
        })
    }
}

#[async_trait]
impl RouteRepository for MemoryRouteRepo {
    async fn create(&self, route: Route) -> Result<Route, DomainError> {
        if self.duplicate_exists(&route, None) {
            return Err(conflict(
                "duplicate match rule under this upstream".to_owned(),
            ));
        }
        self.store.write().insert(route.id.clone(), route.clone());
        Ok(route)
    }

    async fn get(&self, tenant_id: Uuid, id: &str) -> Result<Route, DomainError> {
        self.store
            .read()
            .get(id)
            .filter(|route| route.tenant_id == tenant_id)
            .cloned()
            .ok_or_else(|| not_found("route", id))
    }

    async fn list(&self, tenant_id: Uuid) -> Result<Vec<Route>, DomainError> {
        let mut items: Vec<Route> = self
            .store
            .read()
            .values()
            .filter(|route| route.tenant_id == tenant_id)
            .cloned()
            .collect();
        items.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(items)
    }

    async fn list_by_upstream(
        &self,
        tenant_id: Uuid,
        upstream_id: &str,
    ) -> Result<Vec<Route>, DomainError> {
        let mut items: Vec<Route> = self
            .store
            .read()
            .values()
            .filter(|route| route.tenant_id == tenant_id && route.upstream_id == upstream_id)
            .cloned()
            .collect();
        items.sort_by_key(|route| std::cmp::Reverse(route.priority));
        Ok(items)
    }

    async fn replace(&self, route: Route) -> Result<Route, DomainError> {
        let mut guard = self.store.write();
        let existing = guard
            .get(&route.id)
            .filter(|r| r.tenant_id == route.tenant_id)
            .cloned()
            .ok_or_else(|| not_found("route", &route.id))?;
        let mut stored = route;
        if Self::duplicates(&guard, &stored, Some(&stored.id)) {
            return Err(conflict(
                "duplicate match rule under this upstream".to_owned(),
            ));
        }
        stored.upstream_id = existing.upstream_id;
        guard.insert(stored.id.clone(), stored.clone());
        Ok(stored)
    }

    async fn delete(&self, tenant_id: Uuid, id: &str) -> Result<(), DomainError> {
        let mut guard = self.store.write();
        match guard.get(id) {
            Some(route) if route.tenant_id == tenant_id => {
                guard.remove(id);
                Ok(())
            }
            _ => Err(not_found("route", id)),
        }
    }

    async fn delete_by_upstream(
        &self,
        tenant_id: Uuid,
        upstream_id: &str,
    ) -> Result<(), DomainError> {
        let ids: Vec<String> = self
            .store
            .read()
            .values()
            .filter(|route| route.tenant_id == tenant_id && route.upstream_id == upstream_id)
            .map(|route| route.id.clone())
            .collect();
        let mut guard = self.store.write();
        for id in ids {
            guard.remove(&id);
        }
        Ok(())
    }
}

/// Plugin records, keyed by `(tenant, id)`.
type PluginStore = HashMap<(Uuid, String), Plugin>;

/// In-memory [`PluginRepository`].
#[derive(Clone, Default)]
pub struct MemoryPluginRepo {
    store: Arc<RwLock<PluginStore>>,
}

impl MemoryPluginRepo {
    /// Creates an empty repository.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every stored plugin across tenants (used for reference checks).
    #[must_use]
    pub fn all(&self) -> Vec<Plugin> {
        self.store.read().values().cloned().collect()
    }
}

#[async_trait]
impl PluginRepository for MemoryPluginRepo {
    async fn create(&self, plugin: Plugin) -> Result<Plugin, DomainError> {
        let key = (plugin.tenant_id, plugin.name.clone());
        let mut guard = self.store.write();
        if guard.contains_key(&key) {
            return Err(conflict(format!(
                "plugin name '{}' already exists",
                plugin.name
            )));
        }
        guard.insert(key, plugin.clone());
        Ok(plugin)
    }

    async fn get(&self, tenant_id: Uuid, id: &str) -> Result<Plugin, DomainError> {
        self.store
            .read()
            .values()
            .find(|plugin| plugin.tenant_id == tenant_id && plugin.id == id)
            .cloned()
            .ok_or_else(|| not_found("plugin", id))
    }

    async fn list(&self, tenant_id: Uuid) -> Result<Vec<Plugin>, DomainError> {
        let mut items: Vec<Plugin> = self
            .store
            .read()
            .iter()
            .filter(|((tenant, _), _)| *tenant == tenant_id)
            .map(|(_, v)| v.clone())
            .collect();
        items.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(items)
    }

    async fn delete(&self, tenant_id: Uuid, id: &str) -> Result<(), DomainError> {
        let found = {
            let guard = self.store.read();
            guard
                .iter()
                .find(|((tenant, _), plugin)| *tenant == tenant_id && plugin.id == id)
                .map(|(key, _)| key.clone())
        };
        match found {
            Some(key) => {
                self.store.write().remove(&key);
                Ok(())
            }
            None => Err(not_found("plugin", id)),
        }
    }
}

#[cfg(test)]
#[path = "memory_repo_tests.rs"]
mod tests;
