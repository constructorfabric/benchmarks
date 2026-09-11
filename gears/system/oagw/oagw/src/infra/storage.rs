//! In-process configuration store.
//!
//! OAGW's configuration is small, read-heavy and rebuilt from the types
//! registry on start (`docs/DESIGN.md` §4.7 item 8), so the shipped repository
//! keeps it in memory behind `DashMap`. Tenant scoping is applied on every
//! read and write here, not by the callers.

use std::sync::Arc;

use dashmap::DashMap;
use uuid::Uuid;

use crate::domain::error::{OagwError, OagwResult};
use crate::domain::model::{PluginDef, Route, Upstream};
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};

/// Concurrent, tenant-scoped store for upstreams, routes and plugins.
#[derive(Debug, Default)]
pub struct InMemoryStore {
    upstreams: DashMap<Uuid, Upstream>,
    routes: DashMap<Uuid, Route>,
    plugins: DashMap<Uuid, PluginDef>,
}

impl InMemoryStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    /// Every upstream under `alias` across all tenants, used by the Data
    /// Plane's hierarchy walk.
    #[must_use]
    pub fn find_alias_across_tenants(&self, alias: &str) -> Vec<Upstream> {
        self.upstreams
            .iter()
            .filter(|entry| entry.alias == alias)
            .map(|entry| entry.clone())
            .collect()
    }

    /// Bindings that reference `plugin_uuid`, as `(upstream_ids, route_ids)`.
    #[must_use]
    pub fn references_to_plugin(&self, plugin_uuid: Uuid) -> (Vec<Uuid>, Vec<Uuid>) {
        let upstreams = self
            .upstreams
            .iter()
            .filter(|entry| {
                let auth_match = entry
                    .auth
                    .as_ref()
                    .and_then(|a| a.plugin_type.as_deref())
                    .and_then(crate::domain::gts::instance_uuid)
                    == Some(plugin_uuid);
                let chain_match = entry.plugins.as_ref().is_some_and(|p| {
                    p.items
                        .iter()
                        .any(|item| item.plugin_uuid == Some(plugin_uuid))
                });
                auth_match || chain_match
            })
            .map(|entry| entry.id)
            .collect();

        let routes = self
            .routes
            .iter()
            .filter(|entry| {
                entry.plugins.as_ref().is_some_and(|p| {
                    p.items
                        .iter()
                        .any(|item| item.plugin_uuid == Some(plugin_uuid))
                })
            })
            .map(|entry| entry.id)
            .collect();

        (upstreams, routes)
    }

    /// Delete every plugin whose `gc_eligible_at` is at or before `now_secs`.
    /// Returns the ids collected.
    pub fn collect_garbage(&self, now_secs: u64) -> Vec<Uuid> {
        let due: Vec<Uuid> = self
            .plugins
            .iter()
            .filter(|entry| entry.gc_eligible_at.is_some_and(|at| at <= now_secs))
            .map(|entry| entry.id)
            .collect();
        for id in &due {
            self.plugins.remove(id);
        }
        due
    }

    /// Mark newly unlinked plugins, then delete the ones whose deadline has
    /// passed. Returns the ids collected on this pass.
    ///
    /// Two steps rather than one so a plugin that is unbound and re-bound
    /// inside the TTL window is never collected: re-binding clears the
    /// deadline via [`PluginRepository::touch`].
    pub fn run_gc(&self, now_secs: u64, ttl_secs: u64) -> Vec<Uuid> {
        let unlinked = self.unlinked_plugins();
        for id in &unlinked {
            let already_marked = self
                .plugins
                .get(id)
                .is_some_and(|entry| entry.gc_eligible_at.is_some());
            if !already_marked {
                self.set_gc_eligible_at(*id, Some(now_secs.saturating_add(ttl_secs)));
            }
        }
        // A plugin that came back into use loses its deadline. The ids are
        // collected before mutating: holding a `DashMap` iterator across a
        // write to the same map deadlocks on the shard lock.
        let revived: Vec<Uuid> = self
            .plugins
            .iter()
            .filter(|entry| entry.gc_eligible_at.is_some() && !unlinked.contains(&entry.id))
            .map(|entry| entry.id)
            .collect();
        for id in revived {
            self.set_gc_eligible_at(id, None);
        }

        self.collect_garbage(now_secs)
    }

    /// Ids of stored plugins that no upstream or route references.
    #[must_use]
    pub fn unlinked_plugins(&self) -> Vec<Uuid> {
        self.plugins
            .iter()
            .map(|entry| entry.id)
            .filter(|id| {
                let (upstreams, routes) = self.references_to_plugin(*id);
                upstreams.is_empty() && routes.is_empty()
            })
            .collect()
    }
}

impl UpstreamRepository for InMemoryStore {
    fn insert(&self, upstream: Upstream) -> OagwResult<Upstream> {
        if self
            .upstreams
            .iter()
            .any(|e| e.tenant_id == upstream.tenant_id && e.alias == upstream.alias)
        {
            return Err(OagwError::conflict(format!(
                "an upstream with alias '{}' already exists for this tenant",
                upstream.alias
            ))
            .with("alias", upstream.alias.clone()));
        }
        self.upstreams.insert(upstream.id, upstream.clone());
        Ok(upstream)
    }

    fn replace(&self, upstream: Upstream) -> OagwResult<Upstream> {
        let Some(existing) = self.upstreams.get(&upstream.id).map(|e| e.clone()) else {
            return Err(OagwError::not_found("upstream not found"));
        };
        if existing.tenant_id != upstream.tenant_id {
            return Err(OagwError::not_found("upstream not found"));
        }
        if existing.alias != upstream.alias
            && self
                .upstreams
                .iter()
                .any(|e| e.tenant_id == upstream.tenant_id && e.alias == upstream.alias)
        {
            return Err(OagwError::conflict(format!(
                "an upstream with alias '{}' already exists for this tenant",
                upstream.alias
            )));
        }
        self.upstreams.insert(upstream.id, upstream.clone());
        Ok(upstream)
    }

    fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<Upstream> {
        self.upstreams
            .get(&id)
            .filter(|e| e.tenant_id == tenant_id)
            .map(|e| e.clone())
    }

    fn get_unscoped(&self, id: Uuid) -> Option<Upstream> {
        self.upstreams.get(&id).map(|e| e.clone())
    }

    fn list(&self, tenant_id: Uuid) -> Vec<Upstream> {
        let mut items: Vec<Upstream> = self
            .upstreams
            .iter()
            .filter(|e| e.tenant_id == tenant_id)
            .map(|e| e.clone())
            .collect();
        items.sort_by(|a, b| a.alias.cmp(&b.alias).then(a.id.cmp(&b.id)));
        items
    }

    fn find_by_alias(&self, tenant_id: Uuid, alias: &str) -> Option<Upstream> {
        self.upstreams
            .iter()
            .find(|e| e.tenant_id == tenant_id && e.alias == alias)
            .map(|e| e.clone())
    }

    fn delete(&self, tenant_id: Uuid, id: Uuid) -> bool {
        let owned = self
            .upstreams
            .get(&id)
            .is_some_and(|e| e.tenant_id == tenant_id);
        if owned {
            self.upstreams.remove(&id);
        }
        owned
    }
}

impl RouteRepository for InMemoryStore {
    fn insert(&self, route: Route) -> OagwResult<Route> {
        ensure_match_unique(self, &route)?;
        self.routes.insert(route.id, route.clone());
        Ok(route)
    }

    fn replace(&self, route: Route) -> OagwResult<Route> {
        let Some(existing) = self.routes.get(&route.id).map(|e| e.clone()) else {
            return Err(OagwError::not_found("route not found"));
        };
        if existing.tenant_id != route.tenant_id {
            return Err(OagwError::not_found("route not found"));
        }
        ensure_match_unique(self, &route)?;
        self.routes.insert(route.id, route.clone());
        Ok(route)
    }

    fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<Route> {
        self.routes
            .get(&id)
            .filter(|e| e.tenant_id == tenant_id)
            .map(|e| e.clone())
    }

    fn list(&self, tenant_id: Uuid) -> Vec<Route> {
        let mut items: Vec<Route> = self
            .routes
            .iter()
            .filter(|e| e.tenant_id == tenant_id)
            .map(|e| e.clone())
            .collect();
        items.sort_by(|a, b| b.priority.cmp(&a.priority).then(a.id.cmp(&b.id)));
        items
    }

    fn list_by_upstream(&self, upstream_id: Uuid) -> Vec<Route> {
        let mut items: Vec<Route> = self
            .routes
            .iter()
            .filter(|e| e.upstream_id == upstream_id)
            .map(|e| e.clone())
            .collect();
        items.sort_by(|a, b| b.priority.cmp(&a.priority).then(a.id.cmp(&b.id)));
        items
    }

    fn delete(&self, tenant_id: Uuid, id: Uuid) -> bool {
        let owned = self
            .routes
            .get(&id)
            .is_some_and(|e| e.tenant_id == tenant_id);
        if owned {
            self.routes.remove(&id);
        }
        owned
    }

    fn delete_by_upstream(&self, upstream_id: Uuid) -> usize {
        let doomed: Vec<Uuid> = self
            .routes
            .iter()
            .filter(|e| e.upstream_id == upstream_id)
            .map(|e| e.id)
            .collect();
        for id in &doomed {
            self.routes.remove(id);
        }
        doomed.len()
    }
}

/// No two enabled routes under the same upstream may share
/// `(path, priority)` for the same method.
fn ensure_match_unique(store: &InMemoryStore, candidate: &Route) -> OagwResult<()> {
    let Some(candidate_http) = candidate.http() else {
        return Ok(());
    };
    if !candidate.enabled {
        return Ok(());
    }
    for existing in store.routes.iter() {
        if existing.id == candidate.id
            || existing.upstream_id != candidate.upstream_id
            || !existing.enabled
        {
            continue;
        }
        let Some(existing_http) = existing.http() else {
            continue;
        };
        if existing_http.path != candidate_http.path || existing.priority != candidate.priority {
            continue;
        }
        let overlap = candidate_http
            .methods
            .iter()
            .find(|m| existing_http.allows_method(m));
        if let Some(method) = overlap {
            return Err(OagwError::conflict(format!(
                "route {method} {} at priority {} already exists on this upstream",
                candidate_http.path, candidate.priority
            ))
            .with("conflicting_route_id", existing.id.to_string()));
        }
    }
    Ok(())
}

impl PluginRepository for InMemoryStore {
    fn insert(&self, plugin: PluginDef) -> OagwResult<PluginDef> {
        if self
            .plugins
            .iter()
            .any(|e| e.tenant_id == plugin.tenant_id && e.name == plugin.name)
        {
            return Err(OagwError::conflict(format!(
                "a plugin named '{}' already exists for this tenant",
                plugin.name
            )));
        }
        self.plugins.insert(plugin.id, plugin.clone());
        Ok(plugin)
    }

    fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<PluginDef> {
        self.plugins
            .get(&id)
            .filter(|e| e.tenant_id == tenant_id)
            .map(|e| e.clone())
    }

    fn get_unscoped(&self, id: Uuid) -> Option<PluginDef> {
        self.plugins.get(&id).map(|e| e.clone())
    }

    fn list(&self, tenant_id: Uuid) -> Vec<PluginDef> {
        let mut items: Vec<PluginDef> = self
            .plugins
            .iter()
            .filter(|e| e.tenant_id == tenant_id)
            .map(|e| e.clone())
            .collect();
        items.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
        items
    }

    fn delete(&self, tenant_id: Uuid, id: Uuid) -> bool {
        let owned = self
            .plugins
            .get(&id)
            .is_some_and(|e| e.tenant_id == tenant_id);
        if owned {
            self.plugins.remove(&id);
        }
        owned
    }

    fn touch(&self, id: Uuid, epoch_secs: u64) {
        if let Some(mut entry) = self.plugins.get_mut(&id) {
            entry.last_used_at = Some(epoch_secs);
            entry.gc_eligible_at = None;
        }
    }

    fn set_gc_eligible_at(&self, id: Uuid, epoch_secs: Option<u64>) {
        if let Some(mut entry) = self.plugins.get_mut(&id) {
            entry.gc_eligible_at = epoch_secs;
        }
    }
}

#[cfg(test)]
#[path = "storage_tests.rs"]
mod tests;
