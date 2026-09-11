//! In-memory implementations of the control plane repositories.

use std::sync::Arc;

use dashmap::DashMap;
use parking_lot::RwLock;
use uuid::Uuid;

use crate::domain::alias::aliases_equal;
use crate::domain::error::DomainError;
use crate::domain::model::{Plugin, Route, RouteMatch, Upstream};
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};

fn not_found(kind: &str, id: Uuid) -> DomainError {
    let detail = format!("{kind} '{id}' not found");
    match kind {
        "upstream" => DomainError::UpstreamNotFound(detail),
        "route" => DomainError::RouteNotFound(detail),
        _ => DomainError::PluginNotFound(detail),
    }
}

/// [`UpstreamRepository`] over a [`DashMap`].
#[derive(Debug, Default)]
pub struct MemoryUpstreamRepository {
    entries: DashMap<Uuid, Upstream>,
}

impl UpstreamRepository for MemoryUpstreamRepository {
    fn insert(&self, upstream: Upstream) -> Result<Upstream, DomainError> {
        if self.entries.contains_key(&upstream.id) {
            return Err(DomainError::AliasConflict(format!(
                "upstream '{}' already exists",
                upstream.id
            )));
        }
        self.entries.insert(upstream.id, upstream.clone());
        Ok(upstream)
    }

    fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<Upstream> {
        self.entries
            .get(&id)
            .map(|e| e.value().clone())
            .filter(|u| u.tenant_id == tenant_id)
    }

    fn find_by_alias(&self, tenant_id: Uuid, alias: &str) -> Option<Upstream> {
        self.entries
            .iter()
            .map(|e| e.value().clone())
            .find(|u| u.tenant_id == tenant_id && aliases_equal(&u.alias, alias))
    }

    fn alias_taken(&self, tenant_id: Uuid, alias: &str, excluding: Option<Uuid>) -> bool {
        self.entries.iter().any(|e| {
            let u = e.value();
            u.tenant_id == tenant_id
                && aliases_equal(&u.alias, alias)
                && Some(u.id) != excluding
        })
    }

    fn list(&self, tenant_id: Option<Uuid>) -> Vec<Upstream> {
        self.entries
            .iter()
            .map(|e| e.value().clone())
            .filter(|u| tenant_id.is_none_or(|t| u.tenant_id == t))
            .collect()
    }

    fn update(&self, upstream: Upstream) -> Result<Upstream, DomainError> {
        let existing = self.entries.get(&upstream.id).ok_or_else(|| {
            DomainError::UpstreamNotFound(format!("upstream '{}' does not exist", upstream.id))
        })?;
        if existing.value().tenant_id != upstream.tenant_id {
            return Err(DomainError::UpstreamNotFound(format!(
                "upstream '{}' does not exist",
                upstream.id
            )));
        }
        drop(existing);
        self.entries.insert(upstream.id, upstream.clone());
        Ok(upstream)
    }

    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<Upstream, DomainError> {
        let existing = self.entries.get(&id).filter(|e| e.value().tenant_id == tenant_id);
        match existing {
            Some(e) => {
                let removed = e.value().clone();
                drop(e);
                self.entries.remove(&id);
                Ok(removed)
            }
            None => Err(not_found("upstream", id)),
        }
    }

    fn count(&self) -> usize {
        self.entries.len()
    }
}

/// [`RouteRepository`] over a [`DashMap`].
#[derive(Debug, Default)]
pub struct MemoryRouteRepository {
    entries: DashMap<Uuid, RwLock<Route>>,
}

impl MemoryRouteRepository {
    /// True when two match rules claim the same traffic.
    #[must_use]
    pub fn rules_conflict(candidate: &RouteMatch, existing: &RouteMatch) -> bool {
        match (candidate, existing) {
            (RouteMatch::Http(a), RouteMatch::Http(b)) => {
                a.path == b.path && a.methods.iter().any(|m| b.methods.contains(m))
            }
            (RouteMatch::Grpc(a), RouteMatch::Grpc(b)) => {
                a.service == b.service && a.method == b.method
            }
            _ => false,
        }
    }
}

impl RouteRepository for MemoryRouteRepository {
    fn insert(&self, route: Route) -> Result<Route, DomainError> {
        if self.entries.contains_key(&route.id) {
            return Err(DomainError::MatchConflict(format!(
                "route '{}' already exists",
                route.id
            )));
        }
        self.entries.insert(route.id, RwLock::new(route.clone()));
        Ok(route)
    }

    fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<Route> {
        self.entries
            .get(&id)
            .map(|e| e.value().read().clone())
            .filter(|r| r.tenant_id == tenant_id)
    }

    fn list(&self, tenant_id: Option<Uuid>) -> Vec<Route> {
        self.entries
            .iter()
            .map(|e| e.value().read().clone())
            .filter(|r| tenant_id.is_none_or(|t| r.tenant_id == t))
            .collect()
    }

    fn list_by_upstream(&self, upstream_id: Uuid) -> Vec<Route> {
        self.entries
            .iter()
            .map(|e| e.value().read().clone())
            .filter(|r| r.upstream_id == upstream_id)
            .collect()
    }

    fn update(&self, route: Route) -> Result<Route, DomainError> {
        let entry = self
            .entries
            .get(&route.id)
            .ok_or_else(|| not_found("route", route.id))?;
        if entry.value().read().tenant_id != route.tenant_id {
            return Err(not_found("route", route.id));
        }
        *entry.value().write() = route.clone();
        Ok(route)
    }

    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<Route, DomainError> {
        let entry = self
            .entries
            .get(&id)
            .filter(|e| e.value().read().tenant_id == tenant_id)
            .ok_or_else(|| not_found("route", id))?;
        let removed = entry.value().read().clone();
        drop(entry);
        self.entries.remove(&id);
        Ok(removed)
    }

    fn find_matching(&self, upstream_id: Uuid, route_match: &RouteMatch) -> Option<Route> {
        self.entries
            .iter()
            .map(|e| e.value().read().clone())
            .find(|r| {
                r.upstream_id == upstream_id && Self::rules_conflict(route_match, &r.route_match)
            })
    }

    fn count(&self) -> usize {
        self.entries.len()
    }
}

/// [`PluginRepository`] over a [`DashMap`].
#[derive(Debug, Default)]
pub struct MemoryPluginRepository {
    entries: DashMap<Uuid, Plugin>,
}

impl PluginRepository for MemoryPluginRepository {
    fn insert(&self, plugin: Plugin) -> Result<Plugin, DomainError> {
        if self.entries.contains_key(&plugin.id) {
            return Err(DomainError::Validation(format!(
                "plugin '{}' already exists",
                plugin.id
            )));
        }
        self.entries.insert(plugin.id, plugin.clone());
        Ok(plugin)
    }

    fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<Plugin> {
        self.entries
            .get(&id)
            .map(|e| e.value().clone())
            .filter(|p| p.tenant_id == tenant_id)
    }

    fn list(&self, tenant_id: Option<Uuid>) -> Vec<Plugin> {
        self.entries
            .iter()
            .map(|e| e.value().clone())
            .filter(|p| tenant_id.is_none_or(|t| p.tenant_id == t))
            .collect()
    }

    fn update(&self, plugin: Plugin) -> Result<Plugin, DomainError> {
        let existing = self.entries.get(&plugin.id).ok_or_else(|| not_found("plugin", plugin.id))?;
        if existing.value().tenant_id != plugin.tenant_id {
            return Err(not_found("plugin", plugin.id));
        }
        drop(existing);
        self.entries.insert(plugin.id, plugin.clone());
        Ok(plugin)
    }

    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<Plugin, DomainError> {
        let existing = self.entries.get(&id).filter(|e| e.value().tenant_id == tenant_id);
        match existing {
            Some(e) => {
                let removed = e.value().clone();
                drop(e);
                self.entries.remove(&id);
                Ok(removed)
            }
            None => Err(not_found("plugin", id)),
        }
    }

    fn referencing_resources(&self, _tenant_id: Uuid, _plugin_id: Uuid) -> Vec<String> {
        Vec::new()
    }

    fn count(&self) -> usize {
        self.entries.len()
    }
}

/// All three repositories at once, for wiring the control plane.
pub struct MemoryStores {
    /// Upstream storage.
    pub upstreams: Arc<MemoryUpstreamRepository>,
    /// Route storage.
    pub routes: Arc<MemoryRouteRepository>,
    /// Plugin storage.
    pub plugins: Arc<MemoryPluginRepository>,
}

impl Default for MemoryStores {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryStores {
    /// Shared, empty stores.
    #[must_use]
    pub fn new() -> Self {
        Self {
            upstreams: Arc::new(MemoryUpstreamRepository::default()),
            routes: Arc::new(MemoryRouteRepository::default()),
            plugins: Arc::new(MemoryPluginRepository::default()),
        }
    }
}


