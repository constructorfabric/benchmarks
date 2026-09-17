//! The lock-protected tenant store.
use std::collections::HashMap;
use std::sync::Arc;

use dashmap::DashMap;
use parking_lot::RwLock;
use uuid::Uuid;

use crate::domain::error::{DomainError, PluginReferences};
use crate::domain::model::{Plugin, Route, Upstream};
use crate::domain::repo::ConfigStore;

/// Everything one tenant owns. Guarded by a single lock so cross-collection
/// invariants hold without a second protocol.
#[derive(Debug, Default)]
struct Partition {
    /// `oagw_upstream` keyed by id; the alias is unique per tenant.
    upstreams: HashMap<Uuid, Upstream>,
    /// `oagw_route`, cascade-deleted with its upstream.
    routes: HashMap<Uuid, Route>,
    /// `oagw_plugin` (custom, UUID-backed rows only).
    plugins: HashMap<Uuid, Plugin>,
}

/// In-memory [`ConfigStore`].
///
/// Clone-free handle: the store is cheap to share through an `Arc` and every
/// method takes `&self`.
#[derive(Debug, Default)]
pub struct MemoryStore {
    tenants: DashMap<Uuid, Arc<RwLock<Partition>>>,
}

impl MemoryStore {
    /// Create an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Get (or lazily create) the partition of a tenant. The returned `Arc`
    /// is cloned out so the dashmap shard lock is never held across an
    /// awaited or contended section.
    fn partition(&self, tenant: Uuid) -> Arc<RwLock<Partition>> {
        self.tenants.entry(tenant).or_default().value().clone()
    }

    fn upstream_alias_conflict(partition: &Partition, upstream: &Upstream) -> bool {
        partition
            .upstreams
            .values()
            .any(|existing| existing.alias == upstream.alias)
    }

    fn route_match_taken(
        partition: &Partition,
        tenant: Uuid,
        upstream_id: Uuid,
        match_key: &str,
        except_id: Option<Uuid>,
    ) -> bool {
        partition.routes.values().any(|route| {
            route.tenant_id == tenant
                && route.upstream_id == upstream_id
                && route.id != except_id.unwrap_or_default()
                && route.uniqueness_key() == match_key
        })
    }
}

impl ConfigStore for MemoryStore {
    fn insert_upstream(&self, upstream: &Upstream) -> Result<(), DomainError> {
        let partition = self.partition(upstream.tenant_id);
        let mut guard = partition.write();
        if guard.upstreams.contains_key(&upstream.id) {
            return Err(DomainError::Conflict {
                detail: format!("upstream '{}' already exists", upstream.id),
            });
        }
        if Self::upstream_alias_conflict(&guard, upstream) {
            return Err(DomainError::AliasConflict {
                alias: upstream.alias.clone(),
                tenant_id: upstream.tenant_id,
            });
        }
        guard.upstreams.insert(upstream.id, upstream.clone());
        Ok(())
    }

    fn find_upstream(&self, tenant: Uuid, id: Uuid) -> Result<Option<Upstream>, DomainError> {
        let partition = self.partition(tenant);
        let guard = partition.read();
        Ok(guard
            .upstreams
            .get(&id)
            .filter(|upstream| upstream.tenant_id == tenant)
            .cloned())
    }

    fn find_upstream_by_alias(
        &self,
        tenant: Uuid,
        alias: &str,
    ) -> Result<Option<Upstream>, DomainError> {
        let partition = self.partition(tenant);
        let guard = partition.read();
        Ok(guard
            .upstreams
            .values()
            .find(|upstream| upstream.alias == alias && upstream.tenant_id == tenant)
            .cloned())
    }

    fn replace_upstream(&self, upstream: &Upstream) -> Result<Option<Upstream>, DomainError> {
        let partition = self.partition(upstream.tenant_id);
        let mut guard = partition.write();
        // The alias is immutable, so a replacement can never introduce a
        // conflict; assert it anyway to keep the index honest.
        let replaced = guard.upstreams.get(&upstream.id).cloned();
        if replaced.is_some() {
            if replaced
                .as_ref()
                .is_some_and(|previous| previous.alias != upstream.alias)
            {
                return Err(DomainError::Conflict {
                    detail: "upstream alias is immutable".to_owned(),
                });
            }
            guard.upstreams.insert(upstream.id, upstream.clone());
        }
        Ok(replaced)
    }

    fn delete_upstream(&self, tenant: Uuid, id: Uuid) -> Result<Option<Upstream>, DomainError> {
        let partition = self.partition(tenant);
        let mut guard = partition.write();
        Ok(guard.upstreams.remove(&id))
    }

    fn delete_upstream_cascade(
        &self,
        tenant: Uuid,
        id: Uuid,
    ) -> Result<(Option<Upstream>, usize), DomainError> {
        let partition = self.partition(tenant);
        let mut guard = partition.write();
        let removed = guard.upstreams.remove(&id);
        let cascaded = if removed.is_some() {
            let before = guard.routes.len();
            guard.routes.retain(|_, route| route.upstream_id != id);
            before - guard.routes.len()
        } else {
            0
        };
        Ok((removed, cascaded))
    }

    fn list_upstreams(&self, tenant: Uuid) -> Result<Vec<Upstream>, DomainError> {
        let partition = self.partition(tenant);
        let guard = partition.read();
        Ok(guard.upstreams.values().cloned().collect())
    }

    fn upstream_alias_taken(
        &self,
        tenant: Uuid,
        alias: &str,
        except_id: Option<Uuid>,
    ) -> Result<bool, DomainError> {
        let partition = self.partition(tenant);
        let guard = partition.read();
        Ok(guard.upstreams.values().any(|upstream| {
            upstream.alias == alias && upstream.id != except_id.unwrap_or_default()
        }))
    }

    fn insert_route(&self, route: &Route) -> Result<(), DomainError> {
        let partition = self.partition(route.tenant_id);
        let mut guard = partition.write();
        if guard.routes.contains_key(&route.id) {
            return Err(DomainError::Conflict {
                detail: format!("route '{}' already exists", route.id),
            });
        }
        if Self::route_match_taken(
            &guard,
            route.tenant_id,
            route.upstream_id,
            &route.uniqueness_key(),
            None,
        ) {
            return Err(DomainError::RouteMatchConflict {
                detail: format!(
                    "upstream {} already has a route matching {} at priority {}",
                    route.upstream_id,
                    route.matcher.match_key(),
                    route.priority
                ),
            });
        }
        guard.routes.insert(route.id, route.clone());
        Ok(())
    }

    fn find_route(&self, tenant: Uuid, id: Uuid) -> Result<Option<Route>, DomainError> {
        let partition = self.partition(tenant);
        let guard = partition.read();
        Ok(guard
            .routes
            .get(&id)
            .filter(|route| route.tenant_id == tenant)
            .cloned())
    }

    fn replace_route(&self, route: &Route) -> Result<Option<Route>, DomainError> {
        let partition = self.partition(route.tenant_id);
        let mut guard = partition.write();
        let previous = guard.routes.get(&route.id).cloned();
        if previous.is_some() {
            guard.routes.insert(route.id, route.clone());
        }
        Ok(previous)
    }

    fn delete_route(&self, tenant: Uuid, id: Uuid) -> Result<Option<Route>, DomainError> {
        let partition = self.partition(tenant);
        let mut guard = partition.write();
        Ok(guard.routes.remove(&id))
    }

    fn list_routes(&self, tenant: Uuid) -> Result<Vec<Route>, DomainError> {
        let partition = self.partition(tenant);
        let guard = partition.read();
        Ok(guard.routes.values().cloned().collect())
    }

    fn list_routes_by_upstream(
        &self,
        tenant: Uuid,
        upstream_id: Uuid,
    ) -> Result<Vec<Route>, DomainError> {
        let partition = self.partition(tenant);
        let guard = partition.read();
        Ok(guard
            .routes
            .values()
            .filter(|route| route.upstream_id == upstream_id)
            .cloned()
            .collect())
    }

    fn route_match_key_taken(
        &self,
        tenant: Uuid,
        upstream_id: Uuid,
        match_key: &str,
        except_id: Option<Uuid>,
    ) -> Result<bool, DomainError> {
        let partition = self.partition(tenant);
        let guard = partition.read();
        Ok(Self::route_match_taken(
            &guard,
            tenant,
            upstream_id,
            match_key,
            except_id,
        ))
    }

    fn insert_plugin(&self, plugin: &Plugin) -> Result<(), DomainError> {
        let partition = self.partition(plugin.tenant_id);
        let mut guard = partition.write();
        if guard.plugins.contains_key(&plugin.id) {
            return Err(DomainError::Conflict {
                detail: format!("plugin '{}' already exists", plugin.id),
            });
        }
        if guard
            .plugins
            .values()
            .any(|existing| existing.name == plugin.name)
        {
            return Err(DomainError::Conflict {
                detail: format!(
                    "plugin name '{}' already exists for this tenant",
                    plugin.name
                ),
            });
        }
        guard.plugins.insert(plugin.id, plugin.clone());
        Ok(())
    }

    fn find_plugin(&self, tenant: Uuid, id: Uuid) -> Result<Option<Plugin>, DomainError> {
        let partition = self.partition(tenant);
        let guard = partition.read();
        Ok(guard
            .plugins
            .get(&id)
            .filter(|plugin| plugin.tenant_id == tenant)
            .cloned())
    }

    fn find_plugin_by_name(&self, tenant: Uuid, name: &str) -> Result<Option<Plugin>, DomainError> {
        let partition = self.partition(tenant);
        let guard = partition.read();
        Ok(guard
            .plugins
            .values()
            .find(|plugin| plugin.name == name && plugin.tenant_id == tenant)
            .cloned())
    }

    fn delete_plugin(&self, tenant: Uuid, id: Uuid) -> Result<Option<Plugin>, DomainError> {
        let partition = self.partition(tenant);
        let mut guard = partition.write();
        Ok(guard.plugins.remove(&id))
    }

    fn list_plugins(&self, tenant: Uuid) -> Result<Vec<Plugin>, DomainError> {
        let partition = self.partition(tenant);
        let guard = partition.read();
        Ok(guard.plugins.values().cloned().collect())
    }

    fn plugin_references(
        &self,
        tenant: Uuid,
        plugin_id: Uuid,
    ) -> Result<PluginReferences, DomainError> {
        let partition = self.partition(tenant);
        let guard = partition.read();
        let mut references = PluginReferences::default();
        for upstream in guard.upstreams.values() {
            let via_auth = upstream
                .auth
                .as_ref()
                .is_some_and(|auth| names_plugin(&auth.plugin_uuid, &auth.plugin_type, plugin_id));
            let via_chain = upstream.plugins.as_ref().is_some_and(|plugins| {
                plugins.items.iter().any(|binding| {
                    names_plugin(&binding.plugin_uuid, &binding.plugin_ref, plugin_id)
                })
            });
            if via_auth || via_chain {
                references.upstreams.push(upstream.gts_id());
            }
        }
        for route in guard.routes.values() {
            let via_chain = route.plugins.as_ref().is_some_and(|plugins| {
                plugins.items.iter().any(|binding| {
                    names_plugin(&binding.plugin_uuid, &binding.plugin_ref, plugin_id)
                })
            });
            if via_chain {
                references.routes.push(route.gts_id());
            }
        }
        Ok(references)
    }
}

/// `true` when a stored reference spells the plugin row's identity — either the
/// resolved `plugin_uuid` or an as-yet-unresolved reference naming the row's
/// UUID (bare, or inside its full GTS id).
fn names_plugin(plugin_uuid: &Option<Uuid>, plugin_ref: &str, plugin_id: Uuid) -> bool {
    plugin_uuid.is_some_and(|uuid| uuid == plugin_id)
        || plugin_ref == plugin_id.to_string()
        || plugin_ref.ends_with(&format!("~{plugin_id}"))
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
