//! In-memory repositories for the OAGW control plane.
//!
//! Backed by `DashMap` keyed `(tenant_id, id)` / `(tenant_id, alias)`.
//! Provides snapshot-consistent reads (each lookup clones the stored value)
//! and linearizable upsert/delete via per-key locks.

use std::collections::HashMap;
use std::sync::Arc;

use dashmap::DashMap;
use parking_lot::Mutex;
use uuid::Uuid;

use crate::domain::alias::normalize_alias;
use crate::domain::models::{Plugin, Route, Upstream};
use crate::domain::repo::{PluginRepo, RouteRepo, UpstreamRepo};

/// In-memory upstream repository.
#[derive(Default)]
pub struct InMemoryUpstreamRepo {
    by_id: DashMap<(Uuid, Uuid), Upstream>,
    // (tenant, normalized_alias) -> id — uniqueness guard + by-alias lookup.
    by_alias: DashMap<(Uuid, String), Uuid>,
    lock: Mutex<()>,
}

impl InMemoryUpstreamRepo {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl UpstreamRepo for InMemoryUpstreamRepo {
    fn upsert(&self, tenant_id: Uuid, u: Upstream) -> Result<(), anyhow::Error> {
        let _guard = self.lock.lock();
        let id = u.id.expect("upstream id set during upsert");
        let alias = normalize_alias(&u.alias);
        self.by_id.insert((tenant_id, id), u);
        self.by_alias.insert((tenant_id, alias), id);
        Ok(())
    }

    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, anyhow::Error> {
        let _guard = self.lock.lock();
        let Some(u) = self.by_id.remove(&(tenant_id, id)) else {
            return Ok(false);
        };
        self.by_alias
            .remove(&(tenant_id, normalize_alias(&u.1.alias)));
        Ok(true)
    }

    fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<Arc<Upstream>> {
        self.by_id.get(&(tenant_id, id)).map(|v| Arc::new(v.clone()))
    }

    fn list(&self, tenant_id: Uuid) -> Vec<Arc<Upstream>> {
        let mut out: Vec<Arc<Upstream>> = self
            .by_id
            .iter()
            .filter(|e| e.key().0 == tenant_id)
            .map(|e| Arc::new(e.value().clone()))
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    fn alias_taken(&self, tenant_id: Uuid, alias: &str, except_id: Option<Uuid>) -> bool {
        self.by_alias
            .get(&(tenant_id, normalize_alias(alias)))
            .map(|v| except_id != Some(*v.value()))
            .unwrap_or(false)
    }
}

/// In-memory route repository.
#[derive(Default)]
pub struct InMemoryRouteRepo {
    by_id: DashMap<(Uuid, Uuid), Route>,
    by_upstream: DashMap<(Uuid, Uuid), Vec<Uuid>>,
}

impl InMemoryRouteRepo {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl RouteRepo for InMemoryRouteRepo {
    fn upsert(&self, tenant_id: Uuid, r: Route) -> Result<(), anyhow::Error> {
        let id = r.id.expect("route id set during upsert");
        let upstream = r.upstream_id;
        self.by_id.insert((tenant_id, id), r.clone());
        let mut bucket = self
            .by_upstream
            .entry((tenant_id, upstream))
            .or_insert_with(Vec::new);
        if !bucket.contains(&id) {
            bucket.push(id);
        }
        Ok(())
    }

    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, anyhow::Error> {
        let Some((_, r)) = self.by_id.remove(&(tenant_id, id)) else {
            return Ok(false);
        };
        if let Some(mut bucket) = self.by_upstream.get_mut(&(tenant_id, r.upstream_id)) {
            bucket.retain(|x| *x != id);
        }
        Ok(true)
    }

    fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<Arc<Route>> {
        self.by_id.get(&(tenant_id, id)).map(|v| Arc::new(v.clone()))
    }

    fn list(&self, tenant_id: Uuid) -> Vec<Arc<Route>> {
        let mut out: Vec<Arc<Route>> = self
            .by_id
            .iter()
            .filter(|e| e.key().0 == tenant_id)
            .map(|e| Arc::new(e.value().clone()))
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    fn list_for_upstream(&self, tenant_id: Uuid, upstream_id: Uuid) -> Vec<Arc<Route>> {
        let mut out: Vec<Arc<Route>> = self
            .by_id
            .iter()
            .filter(|e| {
                e.key().0 == tenant_id && e.value().upstream_id == upstream_id
            })
            .map(|e| Arc::new(e.value().clone()))
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }
}

/// In-memory custom plugin repository.
#[derive(Default)]
pub struct InMemoryPluginRepo {
    by_id: DashMap<(Uuid, Uuid), Plugin>,
}

impl InMemoryPluginRepo {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl PluginRepo for InMemoryPluginRepo {
    fn put(&self, tenant_id: Uuid, p: Plugin) -> Result<(), anyhow::Error> {
        let id = p.id.expect("plugin id set during put");
        self.by_id.insert((tenant_id, id), p);
        Ok(())
    }

    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, anyhow::Error> {
        Ok(self.by_id.remove(&(tenant_id, id)).is_some())
    }

    fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<Arc<Plugin>> {
        self.by_id.get(&(tenant_id, id)).map(|v| Arc::new(v.clone()))
    }

    fn list(&self, tenant_id: Uuid) -> Vec<Arc<Plugin>> {
        let mut out: Vec<Arc<Plugin>> = self
            .by_id
            .iter()
            .filter(|e| e.key().0 == tenant_id)
            .map(|e| Arc::new(e.value().clone()))
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }
}

/// Diagnostics helper for tests: dump current contents.
#[allow(dead_code)]
pub(crate) fn snapshot(
    up: &InMemoryUpstreamRepo,
    rt: &InMemoryRouteRepo,
    pl: &InMemoryPluginRepo,
) -> (HashMap<(Uuid, Uuid), Upstream>, HashMap<(Uuid, Uuid), Route>, HashMap<(Uuid, Uuid), Plugin>) {
    (
        up.by_id.iter().map(|e| (*e.key(), e.value().clone())).collect(),
        rt.by_id.iter().map(|e| (*e.key(), e.value().clone())).collect(),
        pl.by_id.iter().map(|e| (*e.key(), e.value().clone())).collect(),
    )
}
