// Updated: 2026-09-01 by Constructor Tech
//! In-memory implementation of the OAGW repositories.
//!
//! OAGW's Control Plane holds configuration that is small (thousands of rows
//! at most), read-heavy and rewritten rarely, and the gear declares no
//! database capability. A single-writer / many-reader map behind a
//! `parking_lot::RwLock` is therefore both the simplest correct choice and a
//! fast one: reads are uncontended, and the Data Plane takes a cheap clone of
//! the matched records before touching the network.
//!
//! Uniqueness rules enforced here (the rest live in the service layer):
//!
//! * `UNIQUE (tenant_id, alias)` on upstreams.
//! * One row per id per collection.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use dashmap::DashMap;
use parking_lot::RwLock;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::repo::{
    PluginRecord, PluginRepository, RouteRecord, RouteRepository, UpstreamRecord,
    UpstreamRepository,
};

// ── Upstreams ───────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct Upstreams {
    by_id: HashMap<Uuid, UpstreamRecord>,
    by_tenant: HashMap<Uuid, HashSet<Uuid>>,
    /// `(tenant, alias) -> id`, enforcing the per-tenant alias uniqueness.
    by_alias: HashMap<(Uuid, String), Uuid>,
    /// `alias -> [(tenant, id)]`, for the Data Plane's cross-tenant lookup.
    alias_global: HashMap<String, Vec<(Uuid, Uuid)>>,
}

/// In-memory upstream repository.
#[derive(Debug, Default)]
pub struct MemoryUpstreamStore {
    inner: RwLock<Upstreams>,
}

impl MemoryUpstreamStore {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn record_of(&self, record: &UpstreamRecord) -> Result<(Uuid, String), DomainError> {
        let id = record
            .upstream
            .id
            .ok_or_else(|| DomainError::Internal("upstream record has no id".to_owned()))?;
        let alias = record
            .upstream
            .alias
            .as_deref()
            .ok_or_else(|| DomainError::Internal("upstream record has no alias".to_owned()))?
            .to_owned();
        Ok((id, alias))
    }
}

#[async_trait]
impl UpstreamRepository for MemoryUpstreamStore {
    async fn insert(&self, record: UpstreamRecord) -> Result<(), DomainError> {
        let (id, alias) = self.record_of(&record)?;
        let mut inner = self.inner.write();
        let key = (record.tenant_id, alias.clone());
        if inner.by_alias.contains_key(&key) {
            return Err(DomainError::Conflict {
                kind: "upstream",
                message: format!("an upstream with alias '{alias}' already exists in this tenant"),
            });
        }
        if inner.by_id.contains_key(&id) {
            return Err(DomainError::Conflict {
                kind: "upstream",
                message: format!("upstream '{id}' already exists"),
            });
        }
        inner
            .by_tenant
            .entry(record.tenant_id)
            .or_default()
            .insert(id);
        inner.by_alias.insert(key, id);
        inner
            .alias_global
            .entry(alias.clone())
            .or_default()
            .push((record.tenant_id, id));
        inner.by_id.insert(id, record);
        Ok(())
    }

    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<UpstreamRecord> {
        let inner = self.inner.read();
        inner
            .by_id
            .get(&id)
            .filter(|r| r.tenant_id == tenant_id)
            .cloned()
    }

    async fn get_by_alias(&self, tenant_id: Uuid, alias: &str) -> Option<UpstreamRecord> {
        let inner = self.inner.read();
        let id = inner.by_alias.get(&(tenant_id, alias.to_owned()))?;
        inner.by_id.get(id).cloned()
    }

    async fn get_by_alias_any_tenant(&self, alias: &str) -> Option<UpstreamRecord> {
        let inner = self.inner.read();
        let owners = inner.alias_global.get(alias)?;
        let (_, id) = owners.first()?;
        inner.by_id.get(id).cloned()
    }

    async fn list(&self, tenant_id: Uuid) -> Vec<UpstreamRecord> {
        let inner = self.inner.read();
        inner
            .by_tenant
            .get(&tenant_id)
            .map(|ids| {
                let mut rows: Vec<UpstreamRecord> = ids
                    .iter()
                    .filter_map(|id| inner.by_id.get(id))
                    .cloned()
                    .collect();
                rows.sort_by(|a, b| a.upstream.alias.cmp(&b.upstream.alias));
                rows
            })
            .unwrap_or_default()
    }

    async fn update(&self, record: UpstreamRecord) -> Result<(), DomainError> {
        let (id, alias) = self.record_of(&record)?;
        let mut inner = self.inner.write();
        let Some(existing) = inner.by_id.get(&id) else {
            return Err(DomainError::upstream_not_found(id));
        };
        if existing.tenant_id != record.tenant_id {
            return Err(DomainError::upstream_not_found(id));
        }
        let old_alias = existing
            .upstream
            .alias
            .clone()
            .expect("stored upstream always has an alias");
        if old_alias != alias {
            let key = (record.tenant_id, alias.clone());
            if inner.by_alias.contains_key(&key) {
                return Err(DomainError::Conflict {
                    kind: "upstream",
                    message: format!(
                        "an upstream with alias '{alias}' already exists in this tenant"
                    ),
                });
            }
            inner
                .by_alias
                .remove(&(record.tenant_id, old_alias.clone()));
            inner.by_alias.insert(key, id);
            if let Some(list) = inner.alias_global.get_mut(&old_alias) {
                list.retain(|(_, x)| *x != id);
                if list.is_empty() {
                    inner.alias_global.remove(&old_alias);
                }
            }
            inner
                .alias_global
                .entry(alias.clone())
                .or_default()
                .push((record.tenant_id, id));
        }
        inner.by_id.insert(id, record);
        Ok(())
    }

    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        let mut inner = self.inner.write();
        let Some(record) = inner.by_id.remove(&id) else {
            return Err(DomainError::upstream_not_found(id));
        };
        if record.tenant_id != tenant_id {
            // Put it back: another tenant must not be able to remove it.
            inner.by_id.insert(id, record);
            return Err(DomainError::upstream_not_found(id));
        }
        if let Some(set) = inner.by_tenant.get_mut(&tenant_id) {
            set.remove(&id);
            if set.is_empty() {
                inner.by_tenant.remove(&tenant_id);
            }
        }
        let alias = record.alias().to_owned();
        inner.by_alias.remove(&(tenant_id, alias.clone()));
        if let Some(list) = inner.alias_global.get_mut(&alias) {
            list.retain(|(_, x)| *x != id);
            if list.is_empty() {
                inner.alias_global.remove(&alias);
            }
        }
        Ok(())
    }

    async fn referencing_plugin(&self, plugin_id: Uuid) -> Vec<UpstreamRecord> {
        let inner = self.inner.read();
        inner
            .by_id
            .values()
            .filter(|r| crate::domain::dto::references_plugin(&r.upstream.plugins, plugin_id))
            .cloned()
            .collect()
    }
}

// ── Routes ──────────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct Routes {
    by_id: HashMap<Uuid, RouteRecord>,
    by_tenant: HashMap<Uuid, HashSet<Uuid>>,
    by_upstream: HashMap<Uuid, Vec<Uuid>>,
}

/// In-memory route repository.
#[derive(Debug, Default)]
pub struct MemoryRouteStore {
    inner: RwLock<Routes>,
}

impl MemoryRouteStore {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

#[async_trait]
impl RouteRepository for MemoryRouteStore {
    async fn insert(&self, record: RouteRecord) -> Result<(), DomainError> {
        let id = record
            .route
            .id
            .ok_or_else(|| DomainError::Internal("route record has no id".to_owned()))?;
        let mut inner = self.inner.write();
        if inner.by_id.contains_key(&id) {
            return Err(DomainError::Conflict {
                kind: "route",
                message: format!("route '{id}' already exists"),
            });
        }
        inner
            .by_tenant
            .entry(record.tenant_id)
            .or_default()
            .insert(id);
        inner
            .by_upstream
            .entry(record.route.upstream_id)
            .or_default()
            .push(id);
        inner.by_id.insert(id, record);
        Ok(())
    }

    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<RouteRecord> {
        let inner = self.inner.read();
        inner
            .by_id
            .get(&id)
            .filter(|r| r.tenant_id == tenant_id)
            .cloned()
    }

    async fn list(&self, tenant_id: Uuid) -> Vec<RouteRecord> {
        let inner = self.inner.read();
        inner
            .by_tenant
            .get(&tenant_id)
            .map(|ids| {
                let mut rows: Vec<RouteRecord> = ids
                    .iter()
                    .filter_map(|id| inner.by_id.get(id))
                    .cloned()
                    .collect();
                rows.sort_by_key(|a| a.route.priority);
                rows
            })
            .unwrap_or_default()
    }

    async fn list_for_upstream(&self, upstream_id: Uuid) -> Vec<RouteRecord> {
        let inner = self.inner.read();
        inner
            .by_upstream
            .get(&upstream_id)
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| inner.by_id.get(id))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    async fn update(&self, record: RouteRecord) -> Result<(), DomainError> {
        let id = record
            .route
            .id
            .ok_or_else(|| DomainError::Internal("route record has no id".to_owned()))?;
        let mut inner = self.inner.write();
        let Some(existing) = inner.by_id.get(&id) else {
            return Err(DomainError::route_not_found(id));
        };
        if existing.tenant_id != record.tenant_id {
            return Err(DomainError::route_not_found(id));
        }
        if existing.route.upstream_id != record.route.upstream_id {
            return Err(DomainError::Conflict {
                kind: "route",
                message: "upstream_id is immutable; create a new route instead".to_owned(),
            });
        }
        inner.by_id.insert(id, record);
        Ok(())
    }

    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        let mut inner = self.inner.write();
        let Some(record) = inner.by_id.remove(&id) else {
            return Err(DomainError::route_not_found(id));
        };
        if record.tenant_id != tenant_id {
            inner.by_id.insert(id, record);
            return Err(DomainError::route_not_found(id));
        }
        if let Some(set) = inner.by_tenant.get_mut(&tenant_id) {
            set.remove(&id);
            if set.is_empty() {
                inner.by_tenant.remove(&tenant_id);
            }
        }
        if let Some(list) = inner.by_upstream.get_mut(&record.route.upstream_id) {
            list.retain(|x| *x != id);
            if list.is_empty() {
                inner.by_upstream.remove(&record.route.upstream_id);
            }
        }
        Ok(())
    }

    async fn referencing_plugin(&self, plugin_id: Uuid) -> Vec<RouteRecord> {
        let inner = self.inner.read();
        inner
            .by_id
            .values()
            .filter(|r| crate::domain::dto::references_plugin(&r.route.plugins, plugin_id))
            .cloned()
            .collect()
    }
}

// ── Plugins ─────────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct Plugins {
    by_id: HashMap<Uuid, PluginRecord>,
    by_tenant: HashMap<Uuid, HashSet<Uuid>>,
}

/// In-memory custom-plugin repository.
#[derive(Debug, Default)]
pub struct MemoryPluginStore {
    inner: RwLock<Plugins>,
}

impl MemoryPluginStore {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

#[async_trait]
impl PluginRepository for MemoryPluginStore {
    async fn insert(&self, record: PluginRecord) -> Result<(), DomainError> {
        let id = record.id();
        let mut inner = self.inner.write();
        if inner.by_id.contains_key(&id) {
            return Err(DomainError::PluginAlreadyExists(id.to_string()));
        }
        inner
            .by_tenant
            .entry(record.tenant_id)
            .or_default()
            .insert(id);
        inner.by_id.insert(id, record);
        Ok(())
    }

    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<PluginRecord> {
        let inner = self.inner.read();
        inner
            .by_id
            .get(&id)
            .filter(|r| r.tenant_id == tenant_id)
            .cloned()
    }

    async fn get_any(&self, id: Uuid) -> Option<PluginRecord> {
        let inner = self.inner.read();
        inner.by_id.get(&id).cloned()
    }

    async fn all_tenants(&self) -> Vec<(Uuid, Uuid)> {
        let inner = self.inner.read();
        inner
            .by_id
            .values()
            .map(|r| (r.tenant_id, r.id()))
            .collect()
    }

    async fn list(&self, tenant_id: Uuid) -> Vec<PluginRecord> {
        let inner = self.inner.read();
        inner
            .by_tenant
            .get(&tenant_id)
            .map(|ids| {
                let mut rows: Vec<PluginRecord> = ids
                    .iter()
                    .filter_map(|id| inner.by_id.get(id))
                    .cloned()
                    .collect();
                rows.sort_by(|a, b| {
                    a.plugin
                        .name
                        .as_deref()
                        .unwrap_or("")
                        .cmp(b.plugin.name.as_deref().unwrap_or(""))
                });
                rows
            })
            .unwrap_or_default()
    }

    async fn update(&self, record: PluginRecord) -> Result<(), DomainError> {
        let id = record.id();
        let mut inner = self.inner.write();
        match inner.by_id.get(&id) {
            None => Err(DomainError::plugin_not_found(id)),
            Some(existing) if existing.tenant_id != record.tenant_id => {
                Err(DomainError::plugin_not_found(id))
            }
            Some(_) => {
                inner.by_id.insert(id, record);
                Ok(())
            }
        }
    }

    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        let mut inner = self.inner.write();
        match inner.by_id.get(&id) {
            None => Err(DomainError::plugin_not_found(id)),
            Some(existing) if existing.tenant_id != tenant_id => {
                Err(DomainError::plugin_not_found(id))
            }
            Some(_) => {
                inner.by_id.remove(&id);
                if let Some(set) = inner.by_tenant.get_mut(&tenant_id) {
                    set.remove(&id);
                    if set.is_empty() {
                        inner.by_tenant.remove(&tenant_id);
                    }
                }
                Ok(())
            }
        }
    }

    async fn collectible(&self, now: SystemTime) -> Vec<PluginRecord> {
        let inner = self.inner.read();
        inner
            .by_id
            .values()
            .filter(|r| r.gc_eligible_at.is_some_and(|t| t <= now))
            .cloned()
            .collect()
    }

    async fn unmark_for_gc(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        let mut inner = self.inner.write();
        match inner.by_id.get_mut(&id) {
            None => Err(DomainError::plugin_not_found(id)),
            Some(existing) if existing.tenant_id != tenant_id => {
                Err(DomainError::plugin_not_found(id))
            }
            Some(existing) => {
                existing.gc_eligible_at = None;
                Ok(())
            }
        }
    }
}

/// Bundle of the three repositories, handed to the service layer as one unit.
#[derive(Debug, Clone)]
pub struct Stores {
    pub upstreams: Arc<MemoryUpstreamStore>,
    pub routes: Arc<MemoryRouteStore>,
    pub plugins: Arc<MemoryPluginStore>,
}

impl Default for Stores {
    fn default() -> Self {
        Self {
            upstreams: MemoryUpstreamStore::new(),
            routes: MemoryRouteStore::new(),
            plugins: MemoryPluginStore::new(),
        }
    }
}

/// The repositories as trait objects, for the service layer.
#[derive(Clone)]
pub struct Repos {
    pub upstreams: Arc<dyn UpstreamRepository>,
    pub routes: Arc<dyn RouteRepository>,
    pub plugins: Arc<dyn PluginRepository>,
}

impl From<&Stores> for Repos {
    fn from(stores: &Stores) -> Self {
        Self {
            upstreams: stores.upstreams.clone(),
            routes: stores.routes.clone(),
            plugins: stores.plugins.clone(),
        }
    }
}

/// Build the [`Repos`] bundle from a [`Stores`] bundle.
#[must_use]
pub fn repos(stores: &Stores) -> Repos {
    Repos::from(stores)
}

/// The `DashMap` dependency is used for the Data Plane's per-tenant rate-limit
/// buckets and circuit breakers; this alias documents the intent.
pub type ConcurrentMap<K, V> = DashMap<K, V>;
