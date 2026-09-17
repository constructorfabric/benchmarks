//! In-memory control-plane store.
//!
//! The gear runs without a database (DESIGN.md "no database"): upstreams,
//! routes and plugins live in process-locked maps, so the control plane is
//! authoritative only for the lifetime of the process. Every write bumps a
//! monotonic generation counter that the data plane (part 2) uses to
//! invalidate its resolved-config caches (ADR 0005).
//!
//! Locking discipline: every method takes the lock it needs, does no I/O and
//! returns an owned value — locks are never held across an `.await`, and
//! [`ManagementService`](crate::domain::services::management::ManagementService)
//! is synchronous by construction.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use arc_swap::ArcSwap;
use parking_lot::RwLock;
use uuid::Uuid;

use crate::domain::error::{DomainError, PluginReferences};
use crate::domain::model::plugin::{Plugin, PluginKind};
use crate::domain::model::route::Route;
use crate::domain::model::upstream::Upstream;
use crate::domain::services::alias::AliasCandidate;
use crate::domain::services::management::ControlPlaneStore;

/// Key of a tenant-scoped resource: `(tenant_id, resource_id)`.
type Key = (Uuid, Uuid);

/// In-memory store for upstreams, routes and plugins.
#[derive(Default)]
pub struct MemoryStorage {
    upstreams: RwLock<BTreeMap<Key, Upstream>>,
    routes: RwLock<BTreeMap<Key, Route>>,
    plugins: RwLock<BTreeMap<Key, Plugin>>,
    generation: ArcSwap<u64>,
}

impl std::fmt::Debug for MemoryStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryStorage")
            .field("upstreams", &self.upstreams.read().len())
            .field("routes", &self.routes.read().len())
            .field("plugins", &self.plugins.read().len())
            .field("generation", &self.generation.load())
            .finish()
    }
}

impl MemoryStorage {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Current configuration generation.
    #[must_use]
    pub fn generation(&self) -> u64 {
        **self.generation.load()
    }

    /// Drop every record (used by tests and by part 2's reset hooks).
    pub fn clear(&self) {
        self.upstreams.write().clear();
        self.routes.write().clear();
        self.plugins.write().clear();
        self.bump();
    }

    fn bump(&self) {
        self.generation.store(Arc::new(self.generation() + 1));
    }

    fn plugin_refs_of_upstream(upstream: &Upstream) -> Vec<String> {
        let mut refs = Vec::new();
        if let Some(auth) = &upstream.auth
            && !auth.plugin.plugin_ref.trim().is_empty()
        {
            refs.push(auth.plugin.plugin_ref.clone());
        }
        if let Some(chain) = &upstream.plugins {
            for item in &chain.items {
                refs.push(item.plugin_ref().to_owned());
            }
        }
        refs
    }

    fn plugin_refs_of_route(route: &Route) -> Vec<String> {
        route
            .plugins
            .as_ref()
            .map(|chain| {
                chain
                    .items
                    .iter()
                    .map(|b| b.plugin_ref().to_owned())
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl ControlPlaneStore for MemoryStorage {
    fn insert_upstream(&self, tenant_id: Uuid, upstream: Upstream) -> Result<(), DomainError> {
        {
            let map = self.upstreams.read();
            if map
                .values()
                .any(|u| u.tenant_id == tenant_id && u.alias == upstream.alias)
            {
                return Err(DomainError::AliasConflict {
                    alias: upstream.alias.clone(),
                });
            }
            if map.contains_key(&(tenant_id, upstream.id)) {
                return Err(DomainError::Internal {
                    diagnostic: format!("upstream {} already exists", upstream.id),
                });
            }
        }
        self.upstreams
            .write()
            .insert((tenant_id, upstream.id), upstream);
        self.bump();
        Ok(())
    }

    fn get_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<Upstream, DomainError> {
        self.upstreams
            .read()
            .get(&(tenant_id, id))
            .cloned()
            .ok_or(DomainError::UpstreamNotFound { id })
    }

    fn find_upstream_by_alias(&self, tenant_id: Uuid, alias: &str) -> Option<Upstream> {
        let wanted = alias.trim().to_ascii_lowercase();
        self.upstreams
            .read()
            .values()
            .filter(|u| u.tenant_id == tenant_id)
            .find(|u| u.alias.eq_ignore_ascii_case(&wanted))
            .cloned()
    }

    fn list_upstreams(&self, tenant_id: Uuid) -> Vec<Upstream> {
        self.upstreams
            .read()
            .range((tenant_id, Uuid::nil())..=(tenant_id, Uuid::max()))
            .map(|(_, v)| v.clone())
            .collect()
    }

    fn update_upstream(&self, tenant_id: Uuid, upstream: Upstream) -> Result<(), DomainError> {
        let mut map = self.upstreams.write();
        if !map.contains_key(&(tenant_id, upstream.id)) {
            return Err(DomainError::UpstreamNotFound { id: upstream.id });
        }
        let collides = map.values().any(|u| {
            u.tenant_id == tenant_id
                && u.id != upstream.id
                && u.alias.eq_ignore_ascii_case(&upstream.alias)
        });
        if collides {
            return Err(DomainError::AliasConflict {
                alias: upstream.alias.clone(),
            });
        }
        map.insert((tenant_id, upstream.id), upstream);
        drop(map);
        self.bump();
        Ok(())
    }

    fn delete_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        let removed = self.upstreams.write().remove(&(tenant_id, id));
        if removed.is_none() {
            return Err(DomainError::UpstreamNotFound { id });
        }
        self.bump();
        Ok(())
    }

    fn alias_candidates(&self, tenant_chain: &[Uuid], alias: &str) -> Vec<AliasCandidate> {
        let wanted = alias.trim().to_ascii_lowercase();
        let map = self.upstreams.read();
        let mut out = Vec::new();
        for tenant_id in tenant_chain {
            if let Some(upstream) = map
                .values()
                .find(|u| u.tenant_id == *tenant_id && u.alias.eq_ignore_ascii_case(&wanted))
            {
                out.push(AliasCandidate {
                    tenant_id: upstream.tenant_id,
                    alias: upstream.alias.clone(),
                    upstream_id: upstream.id,
                    enabled: upstream.enabled,
                });
            }
        }
        out
    }

    fn insert_route(&self, tenant_id: Uuid, route: Route) -> Result<(), DomainError> {
        let mut map = self.routes.write();
        if map.contains_key(&(tenant_id, route.id)) {
            return Err(DomainError::Internal {
                diagnostic: format!("route {} already exists", route.id),
            });
        }
        map.insert((tenant_id, route.id), route);
        drop(map);
        self.bump();
        Ok(())
    }

    fn get_route(&self, tenant_id: Uuid, id: Uuid) -> Result<Route, DomainError> {
        self.routes
            .read()
            .get(&(tenant_id, id))
            .cloned()
            .ok_or(DomainError::RouteNotFound {
                detail: format!("route {id} does not exist in this tenant"),
            })
    }

    fn list_routes(&self, tenant_id: Uuid) -> Vec<Route> {
        self.routes
            .read()
            .range((tenant_id, Uuid::nil())..=(tenant_id, Uuid::max()))
            .map(|(_, v)| v.clone())
            .collect()
    }

    fn update_route(&self, tenant_id: Uuid, route: Route) -> Result<(), DomainError> {
        let mut map = self.routes.write();
        if !map.contains_key(&(tenant_id, route.id)) {
            return Err(DomainError::RouteNotFound {
                detail: format!("route {} does not exist in this tenant", route.id),
            });
        }
        map.insert((tenant_id, route.id), route);
        drop(map);
        self.bump();
        Ok(())
    }

    fn delete_route(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        let removed = self.routes.write().remove(&(tenant_id, id));
        if removed.is_none() {
            return Err(DomainError::RouteNotFound {
                detail: format!("route {id} does not exist in this tenant"),
            });
        }
        self.bump();
        Ok(())
    }

    fn insert_plugin(&self, tenant_id: Uuid, plugin: Plugin) -> Result<(), DomainError> {
        let mut map = self.plugins.write();
        if map.contains_key(&(tenant_id, plugin.id)) {
            return Err(DomainError::Internal {
                diagnostic: format!("plugin {} already exists", plugin.id),
            });
        }
        map.insert((tenant_id, plugin.id), plugin);
        drop(map);
        self.bump();
        Ok(())
    }

    fn get_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<Plugin, DomainError> {
        self.plugins
            .read()
            .get(&(tenant_id, id))
            .cloned()
            .ok_or(DomainError::PluginNotFound { id })
    }

    fn list_plugins(&self, tenant_id: Uuid) -> Vec<Plugin> {
        self.plugins
            .read()
            .range((tenant_id, Uuid::nil())..=(tenant_id, Uuid::max()))
            .map(|(_, v)| v.clone())
            .collect()
    }

    fn delete_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        let removed = self.plugins.write().remove(&(tenant_id, id));
        if removed.is_none() {
            return Err(DomainError::PluginNotFound { id });
        }
        self.bump();
        Ok(())
    }

    fn plugin_references(&self, tenant_id: Uuid, plugin_id: Uuid) -> PluginReferences {
        let mut refs = PluginReferences::default();
        let mut upstreams = BTreeSet::new();
        let mut routes = BTreeSet::new();
        // The kind only matters for the full-GTS-id form of a reference; a
        // deleted plugin's kind is read back from the store when it is still
        // there (the caller normally resolves it first).
        let kind = self
            .get_plugin(tenant_id, plugin_id)
            .map_or(PluginKind::Auth, |plugin| plugin.plugin_type);
        for upstream in self.list_upstreams(tenant_id) {
            for plugin_ref in Self::plugin_refs_of_upstream(&upstream) {
                if plugin_ref_names(&plugin_ref, plugin_id, kind) {
                    upstreams.insert(upstream.gts_id());
                    break;
                }
            }
        }
        for route in self.list_routes(tenant_id) {
            for plugin_ref in Self::plugin_refs_of_route(&route) {
                if plugin_ref_names(&plugin_ref, plugin_id, kind) {
                    routes.insert(route.gts_id());
                    break;
                }
            }
        }
        refs.upstreams = upstreams.into_iter().collect();
        refs.routes = routes.into_iter().collect();
        refs
    }

    fn generation(&self) -> u64 {
        MemoryStorage::generation(self)
    }
}

/// True when a `plugin_ref` string names the UUID-backed plugin `plugin_id`.
///
/// The reference may be the bare UUID, the full GTS instance id of the plugin,
/// or the bare string with a trailing instance id (the `type~instance` form).
fn plugin_ref_names(plugin_ref: &str, plugin_id: Uuid, kind: PluginKind) -> bool {
    let bare = plugin_id.to_string();
    let gts = crate::domain::gts_helpers::gts_instance_id(kind.type_id(), &plugin_id);
    plugin_ref == bare
        || plugin_ref == gts
        || plugin_ref.ends_with(&bare)
        || plugin_ref.trim_end_matches(&bare).ends_with('~')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_bumps_on_writes() {
        let store = MemoryStorage::new();
        assert_eq!(store.generation(), 0);
        let upstream = Upstream {
            id: Uuid::new_v4(),
            ..Upstream::default()
        };
        store.insert_upstream(Uuid::new_v4(), upstream).unwrap();
        assert_eq!(store.generation(), 1);
        store.clear();
        assert_eq!(store.generation(), 2);
    }

    #[test]
    fn alias_lookup_is_case_insensitive() {
        let store = MemoryStorage::new();
        let tenant = Uuid::new_v4();
        let upstream = Upstream {
            tenant_id: tenant,
            alias: "api.openai.com".to_owned(),
            ..Upstream::default()
        };
        store.insert_upstream(tenant, upstream).unwrap();
        assert!(
            store
                .find_upstream_by_alias(tenant, "API.OpenAI.COM")
                .is_some()
        );
        assert!(store.find_upstream_by_alias(tenant, "other").is_none());
    }
}
