//! In-memory control-plane storage.
//!
//! The OAGW dependency set ships no `toolkit-db`, so the control plane keeps
//! upstreams/routes/plugins in process memory (DashMap), keyed by tenant id.
//! This is documented as a deviation from the DESIGN relational baseline:
//! configuration is not durable across restarts.

use std::sync::Arc;

use dashmap::DashMap;
use uuid::Uuid;

use crate::domain::models::{Plugin, Route, Stored, Upstream};

/// In-memory control-plane store.
#[derive(Default)]
pub struct ControlPlaneStore {
    /// Upstreams keyed by `(tenant_id, upstream_id)`.
    upstreams: DashMap<(Uuid, Uuid), Stored<Upstream>>,
    /// Alias index: `(tenant_id, normalized_alias) -> upstream_id`.
    upstream_alias: DashMap<(Uuid, String), Uuid>,
    /// Routes keyed by `(tenant_id, route_id)`.
    routes: DashMap<(Uuid, Uuid), Stored<Route>>,
    /// Route index by upstream: `(tenant_id, upstream_id) -> Vec<route_id>`.
    routes_by_upstream: DashMap<(Uuid, Uuid), Vec<Uuid>>,
    /// Plugins keyed by `(tenant_id, plugin_id)`.
    plugins: DashMap<(Uuid, Uuid), Stored<Plugin>>,
    /// Plugin name index: `(tenant_id, name) -> plugin_id`.
    plugin_name: DashMap<(Uuid, String), Uuid>,
}

impl ControlPlaneStore {
    /// Create an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    // ------------------------------------------------------------------
    // Upstreams
    // ------------------------------------------------------------------

    /// Insert an upstream; returns `Ok(())` or `Err(())` on alias conflict.
    pub fn insert_upstream(&self, stored: Stored<Upstream>) -> Result<(), ()> {
        let upstream = &stored.record;
        let id = upstream.id.expect("upstream id assigned before store");
        let tenant = stored.tenant_id;
        let alias = upstream
            .alias
            .clone()
            .expect("upstream alias enforced before store");

        if self
            .upstream_alias
            .insert((tenant, alias.clone()), id)
            .is_some()
            && !self.upstreams.contains_key(&(tenant, id))
        {
            // Alias already claimed by another upstream.
            return Err(());
        }
        self.upstreams.insert((tenant, id), stored);
        Ok(())
    }

    /// Replace a stored upstream (alias index refreshed by the caller).
    pub fn replace_upstream(&self, stored: Stored<Upstream>) {
        let id = stored.record.id.expect("upstream id present on replace");
        let tenant = stored.tenant_id;
        self.upstreams.insert((tenant, id), stored);
    }

    /// Remove an upstream and its routes. Returns the removed upstream.
    pub fn remove_upstream(&self, tenant_id: Uuid, id: Uuid) -> Option<Stored<Upstream>> {
        let removed = self.upstreams.remove(&(tenant_id, id)).map(|(_, v)| v);
        if let Some(u) = &removed {
            if let Some(alias) = &u.record.alias {
                if self
                    .upstream_alias
                    .get(&(tenant_id, alias.clone()))
                    .map(|v| *v == id)
                    .unwrap_or(false)
                {
                    self.upstream_alias.remove(&(tenant_id, alias.clone()));
                }
            }
        }
        // Cascade-delete routes owned by this upstream.
        if let Some(route_ids) = self.routes_by_upstream.remove(&(tenant_id, id)) {
            for rid in route_ids.1 {
                self.routes.remove(&(tenant_id, rid));
            }
        }
        removed
    }

    /// Look up an upstream by `(tenant_id, alias)` (exact tenant — the proxy
    /// walks the ancestor chain itself).
    pub fn upstream_by_alias(&self, tenant_id: Uuid, alias: &str) -> Option<Stored<Upstream>> {
        let id = self
            .upstream_alias
            .get(&(tenant_id, alias.to_owned()))?
            .value()
            .clone();
        self.upstreams
            .get(&(tenant_id, id))
            .map(|e| e.value().clone())
    }

    /// Get an upstream by id.
    pub fn upstream_by_id(&self, tenant_id: Uuid, id: Uuid) -> Option<Stored<Upstream>> {
        self.upstreams
            .get(&(tenant_id, id))
            .map(|e| e.value().clone())
    }

    /// List the calling tenant's own upstreams.
    pub fn list_upstreams(&self, tenant_id: Uuid) -> Vec<Stored<Upstream>> {
        self.upstreams
            .iter()
            .filter(|e| e.key().0 == tenant_id)
            .map(|e| e.value().clone())
            .collect()
    }

    // ------------------------------------------------------------------
    // Routes
    // ------------------------------------------------------------------

    /// Insert a route under its upstream.
    pub fn insert_route(&self, stored: Stored<Route>) {
        let id = stored.record.id.expect("route id assigned before store");
        let tenant = stored.tenant_id;
        let up_id = stored.record.upstream_id;
        self.routes.insert((tenant, id), stored);
        self.routes_by_upstream
            .entry((tenant, up_id))
            .or_default()
            .push(id);
    }

    /// Replace a stored route.
    pub fn replace_route(&self, stored: Stored<Route>) {
        let id = stored.record.id.expect("route id present on replace");
        self.routes.insert((stored.tenant_id, id), stored);
    }

    /// Remove a route.
    pub fn remove_route(&self, tenant_id: Uuid, id: Uuid) -> Option<Stored<Route>> {
        let removed = self.routes.remove(&(tenant_id, id)).map(|(_, v)| v);
        if let Some(r) = &removed {
            let up_id = r.record.upstream_id;
            if let Some(mut ids) = self.routes_by_upstream.get_mut(&(tenant_id, up_id)) {
                ids.retain(|rid| *rid != id);
            }
        }
        removed
    }

    /// List the calling tenant's own routes.
    pub fn list_routes(&self, tenant_id: Uuid) -> Vec<Stored<Route>> {
        self.routes
            .iter()
            .filter(|e| e.key().0 == tenant_id)
            .map(|e| e.value().clone())
            .collect()
    }

    /// List routes for a given upstream (any owning tenant for proxy use).
    pub fn routes_for_upstream(&self, upstream_id: Uuid) -> Vec<Stored<Route>> {
        self.routes
            .iter()
            .filter(|e| e.value().record.upstream_id == upstream_id)
            .map(|e| e.value().clone())
            .collect()
    }

    /// Get a route by id.
    pub fn route_by_id(&self, tenant_id: Uuid, id: Uuid) -> Option<Stored<Route>> {
        self.routes.get(&(tenant_id, id)).map(|e| e.value().clone())
    }

    // ------------------------------------------------------------------
    // Plugins
    // ------------------------------------------------------------------

    /// Insert a plugin; returns `Ok(())` or `Err(())` on name conflict.
    pub fn insert_plugin(&self, stored: Stored<Plugin>) -> Result<(), ()> {
        let id = stored.record.id.expect("plugin id assigned before store");
        let tenant = stored.tenant_id;
        let name = stored.record.name.clone();
        if self.plugin_name.insert((tenant, name), id).is_some() {
            return Err(());
        }
        self.plugins.insert((tenant, id), stored);
        Ok(())
    }

    /// Remove a plugin (caller verified it is unlinked first).
    pub fn remove_plugin(&self, tenant_id: Uuid, id: Uuid) -> Option<Stored<Plugin>> {
        let removed = self.plugins.remove(&(tenant_id, id)).map(|(_, v)| v);
        if let Some(p) = &removed {
            self.plugin_name.remove(&(tenant_id, p.record.name.clone()));
        }
        removed
    }

    /// Get a plugin by id.
    pub fn plugin_by_id(&self, tenant_id: Uuid, id: Uuid) -> Option<Stored<Plugin>> {
        self.plugins
            .get(&(tenant_id, id))
            .map(|e| e.value().clone())
    }

    /// Look up a plugin by name.
    pub fn plugin_by_name(&self, tenant_id: Uuid, name: &str) -> Option<Stored<Plugin>> {
        let id = self
            .plugin_name
            .get(&(tenant_id, name.to_owned()))?
            .value()
            .clone();
        self.plugins
            .get(&(tenant_id, id))
            .map(|e| e.value().clone())
    }

    /// List the calling tenant's own plugins.
    pub fn list_plugins(&self, tenant_id: Uuid) -> Vec<Stored<Plugin>> {
        self.plugins
            .iter()
            .filter(|e| e.key().0 == tenant_id)
            .map(|e| e.value().clone())
            .collect()
    }
}

/// Convenience alias for a shared store handle.
pub type SharedStore = Arc<ControlPlaneStore>;
