//! In-process storage for the OAGW control plane.
//!
//! Configuration is held in memory keyed by tenant, which matches the
//! single-instance deployment model of the example server: the control plane
//! writes and the data plane reads from the same process, so a request is
//! always routed with the configuration it was configured with. Persistence
//! is deliberately not wired to `toolkit-db` here; `domain::repo` keeps the
//! seam so a SeaORM-backed implementation can replace this module without
//! touching the control or data planes.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::RwLock;
use uuid::Uuid;

use crate::domain::model::{Plugin, Route, Upstream};
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};

#[derive(Debug, Default)]
struct Store {
    upstreams: HashMap<Uuid, Upstream>,
    routes: HashMap<Uuid, Route>,
    plugins: HashMap<Uuid, Plugin>,
}

/// Shared in-memory store handle.
#[derive(Debug, Clone, Default)]
pub struct MemoryStore {
    inner: Arc<RwLock<Store>>,
}

impl MemoryStore {
    /// Create an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of stored upstreams (used by tests).
    #[must_use]
    pub fn upstream_count(&self) -> usize {
        self.inner.read().upstreams.len()
    }
}

#[async_trait]
impl UpstreamRepository for MemoryStore {
    async fn insert(&self, upstream: Upstream) -> Result<(), anyhow::Error> {
        self.inner.write().upstreams.insert(upstream.id, upstream);
        Ok(())
    }

    async fn update(&self, upstream: Upstream) -> Result<(), anyhow::Error> {
        let mut guard = self.inner.write();
        if !guard.upstreams.contains_key(&upstream.id) {
            anyhow::bail!("upstream {} not found", upstream.id);
        }
        guard.upstreams.insert(upstream.id, upstream);
        Ok(())
    }

    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Upstream>, anyhow::Error> {
        Ok(self
            .inner
            .read()
            .upstreams
            .get(&id)
            .filter(|u| u.tenant_id == tenant_id)
            .cloned())
    }

    async fn find_by_alias(
        &self,
        tenant_id: Uuid,
        alias: &str,
    ) -> Result<Option<Upstream>, anyhow::Error> {
        Ok(self
            .inner
            .read()
            .upstreams
            .values()
            .find(|u| u.tenant_id == tenant_id && u.alias == alias)
            .cloned())
    }

    async fn list(&self, tenant_id: Uuid) -> Result<Vec<Upstream>, anyhow::Error> {
        let mut out: Vec<Upstream> = self
            .inner
            .read()
            .upstreams
            .values()
            .filter(|u| u.tenant_id == tenant_id)
            .cloned()
            .collect();
        out.sort_by(|a, b| a.alias.cmp(&b.alias).then_with(|| a.id.cmp(&b.id)));
        Ok(out)
    }

    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, anyhow::Error> {
        let mut guard = self.inner.write();
        match guard.upstreams.get(&id) {
            Some(u) if u.tenant_id == tenant_id => {
                guard.upstreams.remove(&id);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn alias_taken(
        &self,
        tenant_id: Uuid,
        alias: &str,
        exclude_id: Option<Uuid>,
    ) -> Result<bool, anyhow::Error> {
        Ok(self.inner.read().upstreams.values().any(|u| {
            u.tenant_id == tenant_id && u.alias == alias && Some(u.id) != exclude_id
        }))
    }
}

#[async_trait]
impl RouteRepository for MemoryStore {
    async fn insert(&self, route: Route) -> Result<(), anyhow::Error> {
        self.inner.write().routes.insert(route.id, route);
        Ok(())
    }

    async fn update(&self, route: Route) -> Result<(), anyhow::Error> {
        let mut guard = self.inner.write();
        if !guard.routes.contains_key(&route.id) {
            anyhow::bail!("route {} not found", route.id);
        }
        guard.routes.insert(route.id, route);
        Ok(())
    }

    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Route>, anyhow::Error> {
        Ok(self
            .inner
            .read()
            .routes
            .get(&id)
            .filter(|r| r.tenant_id == tenant_id)
            .cloned())
    }

    async fn list(
        &self,
        tenant_id: Uuid,
        upstream_id: Option<Uuid>,
    ) -> Result<Vec<Route>, anyhow::Error> {
        let mut out: Vec<Route> = self
            .inner
            .read()
            .routes
            .values()
            .filter(|r| r.tenant_id == tenant_id && upstream_id.is_none_or(|u| r.upstream_id == u))
            .cloned()
            .collect();
        out.sort_by_key(|a| a.id);
        Ok(out)
    }

    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, anyhow::Error> {
        let mut guard = self.inner.write();
        match guard.routes.get(&id) {
            Some(r) if r.tenant_id == tenant_id => {
                guard.routes.remove(&id);
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}

#[async_trait]
impl PluginRepository for MemoryStore {
    async fn insert(&self, plugin: Plugin) -> Result<(), anyhow::Error> {
        self.inner.write().plugins.insert(plugin.id, plugin);
        Ok(())
    }

    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Plugin>, anyhow::Error> {
        Ok(self
            .inner
            .read()
            .plugins
            .get(&id)
            .filter(|p| p.tenant_id == tenant_id)
            .cloned())
    }

    async fn list(&self, tenant_id: Uuid) -> Result<Vec<Plugin>, anyhow::Error> {
        let mut out: Vec<Plugin> = self
            .inner
            .read()
            .plugins
            .values()
            .filter(|p| p.tenant_id == tenant_id)
            .cloned()
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
        Ok(out)
    }

    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, anyhow::Error> {
        let mut guard = self.inner.write();
        match guard.plugins.get(&id) {
            Some(p) if p.tenant_id == tenant_id => {
                guard.plugins.remove(&id);
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}
