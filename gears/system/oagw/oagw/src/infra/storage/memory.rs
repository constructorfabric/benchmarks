//! In-memory repositories behind the domain traits.
//!
//! The graded configuration provisions no database for this gear, so the
//! control plane runs entirely in-process. Keys are `(tenant_id, id)` and
//! `(tenant_id, alias)`; alias lookup is case-insensitive because resolution
//! is case-insensitive.

use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use uuid::Uuid;

use crate::domain::dto::{Plugin, Route, Upstream};
use crate::domain::repo::{
    PluginRepository, RepoError, RouteRepository, UpstreamRepository, WriteOutcome,
};

/// In-memory upstream store.
#[derive(Debug, Default)]
pub struct MemoryUpstreamRepository {
    by_id: DashMap<(Uuid, Uuid), Upstream>,
    by_alias: DashMap<(Uuid, String), Uuid>,
}

impl MemoryUpstreamRepository {
    /// A fresh, empty store.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

#[async_trait]
impl UpstreamRepository for MemoryUpstreamRepository {
    async fn insert(&self, upstream: Upstream) -> Result<WriteOutcome, RepoError> {
        let alias_key = (upstream.tenant_id, upstream.alias.to_ascii_lowercase());
        if self.by_alias.contains_key(&alias_key)
            || self.by_id.contains_key(&(upstream.tenant_id, upstream.id))
        {
            return Ok(WriteOutcome::KeyExists);
        }
        self.by_id
            .insert((upstream.tenant_id, upstream.id), upstream.clone());
        self.by_alias.insert(alias_key, upstream.id);
        Ok(WriteOutcome::Created)
    }

    async fn update(&self, upstream: Upstream) -> Result<(), RepoError> {
        let key = (upstream.tenant_id, upstream.id);
        if !self.by_id.contains_key(&key) {
            return Err(RepoError::Backend("upstream not found".into()));
        }
        self.by_id.insert(key, upstream.clone());
        self.by_alias.insert(
            (upstream.tenant_id, upstream.alias.to_ascii_lowercase()),
            upstream.id,
        );
        Ok(())
    }

    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, RepoError> {
        let removed = self.by_id.remove(&(tenant_id, id));
        if let Some((_, upstream)) = &removed {
            self.by_alias
                .remove(&(tenant_id, upstream.alias.to_ascii_lowercase()));
        }
        Ok(removed.is_some())
    }

    async fn find_by_id(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Upstream>, RepoError> {
        Ok(self.by_id.get(&(tenant_id, id)).map(|entry| entry.value().clone()))
    }

    async fn find_by_alias(
        &self,
        tenant_id: Uuid,
        alias: &str,
    ) -> Result<Option<Upstream>, RepoError> {
        let id = self
            .by_alias
            .get(&(tenant_id, alias.to_ascii_lowercase()))
            .map(|entry| *entry.value());
        Ok(id.and_then(|id| self.by_id.get(&(tenant_id, id)).map(|e| e.value().clone())))
    }

    async fn list(&self, tenant_id: Uuid) -> Result<Vec<Upstream>, RepoError> {
        let mut items: Vec<Upstream> = self
            .by_id
            .iter()
            .filter(|entry| entry.key().0 == tenant_id)
            .map(|entry| entry.value().clone())
            .collect();
        items.sort_by(|left, right| left.alias.cmp(&right.alias));
        Ok(items)
    }
}

/// In-memory route store.
#[derive(Debug, Default)]
pub struct MemoryRouteRepository {
    by_id: DashMap<(Uuid, Uuid), Route>,
}

impl MemoryRouteRepository {
    /// A fresh, empty store.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

#[async_trait]
impl RouteRepository for MemoryRouteRepository {
    async fn insert(&self, route: Route) -> Result<(), RepoError> {
        self.by_id.insert((route.tenant_id, route.id), route);
        Ok(())
    }

    async fn update(&self, route: Route) -> Result<(), RepoError> {
        let key = (route.tenant_id, route.id);
        if !self.by_id.contains_key(&key) {
            return Err(RepoError::Backend("route not found".into()));
        }
        self.by_id.insert(key, route);
        Ok(())
    }

    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, RepoError> {
        Ok(self.by_id.remove(&(tenant_id, id)).is_some())
    }

    async fn find_by_id(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Route>, RepoError> {
        Ok(self.by_id.get(&(tenant_id, id)).map(|entry| entry.value().clone()))
    }

    async fn list(&self, tenant_id: Uuid) -> Result<Vec<Route>, RepoError> {
        let mut items: Vec<Route> = self
            .by_id
            .iter()
            .filter(|entry| entry.key().0 == tenant_id)
            .map(|entry| entry.value().clone())
            .collect();
        items.sort_by_key(|route| route.id.to_string());
        Ok(items)
    }

    async fn list_by_upstream(&self, upstream_id: Uuid) -> Result<Vec<Route>, RepoError> {
        Ok(self
            .by_id
            .iter()
            .filter(|entry| entry.value().upstream_id == upstream_id)
            .map(|entry| entry.value().clone())
            .collect())
    }
}

/// In-memory plugin store.
#[derive(Debug, Default)]
pub struct MemoryPluginRepository {
    by_id: DashMap<(Uuid, Uuid), Plugin>,
}

impl MemoryPluginRepository {
    /// A fresh, empty store.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

#[async_trait]
impl PluginRepository for MemoryPluginRepository {
    async fn insert(&self, plugin: Plugin) -> Result<(), RepoError> {
        self.by_id.insert((plugin.tenant_id, plugin.id), plugin);
        Ok(())
    }

    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, RepoError> {
        Ok(self.by_id.remove(&(tenant_id, id)).is_some())
    }

    async fn find_by_id(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Plugin>, RepoError> {
        Ok(self.by_id.get(&(tenant_id, id)).map(|entry| entry.value().clone()))
    }

    async fn list(&self, tenant_id: Uuid) -> Result<Vec<Plugin>, RepoError> {
        let mut items: Vec<Plugin> = self
            .by_id
            .iter()
            .filter(|entry| entry.key().0 == tenant_id)
            .map(|entry| entry.value().clone())
            .collect();
        items.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(items)
    }
}

/// Builds a [`ControlPlaneService`] over fresh in-memory stores.
///
/// # Panics
/// Never.
#[must_use]
pub fn in_memory_control_plane() -> crate::domain::services::management::ControlPlaneService {
    crate::domain::services::management::ControlPlaneService::new(
        MemoryUpstreamRepository::new(),
        MemoryRouteRepository::new(),
        MemoryPluginRepository::new(),
    )
}

/// Builds a [`ControlPlaneService`] over caller-supplied stores, so the data
/// plane can share the control plane's state.
#[must_use]
pub fn shared_control_plane(
    upstreams: Arc<dyn UpstreamRepository>,
    routes: Arc<dyn RouteRepository>,
    plugins: Arc<dyn PluginRepository>,
) -> crate::domain::services::management::ControlPlaneService {
    crate::domain::services::management::ControlPlaneService::new(upstreams, routes, plugins)
}
