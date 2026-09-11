//! The in-memory plugin store
//! (`cpt-cf-oagw-algo-inmemory-repository`, `cpt-cf-oagw-dod-plugin-identity`).
//!
//! A stored plugin is a Starlark custom plugin: its composite key is
//! `(tenant_id, id)` and its natural key is `(tenant_id, name)`, the plugin
//! uniqueness invariant of `cpt-cf-oagw-db-schema`. Deleting one removes every
//! binding that still references it.

// @cpt-begin:cpt-cf-oagw-dod-inmemory-repos:p1:inst-full

use std::sync::Arc;

use parking_lot::RwLock;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::{Plugin, PluginsConfig};
use crate::domain::repo::{PluginReference, PluginRepository, ResourceLifecycle, tenant_scope};
use crate::domain::validation::validate_plugin_aggregate;

use super::{Inner, ResourceKind, Stored, already_exists, lookup, not_found};

/// The thread-safe in-memory plugin store.
#[derive(Debug, Clone)]
pub struct InMemoryPluginRepository {
    inner: Arc<RwLock<Inner>>,
}

impl InMemoryPluginRepository {
    pub(crate) fn new(inner: Arc<RwLock<Inner>>) -> Self {
        Self { inner }
    }
}

impl PluginRepository for InMemoryPluginRepository {
    fn insert(&self, plugin: &Plugin) -> Result<Plugin, DomainError> {
        // The candidate is validated before any mutation is applied.
        validate_plugin_aggregate(plugin)?;
        let tenant_id = tenant_scope(plugin.tenant_id)?;
        let id = plugin.id.unwrap_or_default();
        let name = plugin.name.clone().unwrap_or_default();
        let mut inner = self.inner.write();
        let taken_id = inner.plugins.contains_key(&(tenant_id, id))
            || inner
                .deleted
                .contains_key(&(ResourceKind::Plugin, tenant_id, id));
        let taken_name = inner.plugin_names.contains_key(&(tenant_id, name.clone()));
        if taken_id || taken_name {
            return Err(already_exists(
                if taken_id { "id" } else { "name" },
                "identifier or name",
            ));
        }
        // A plugin carries no `enabled` field, so it enters `Active` directly.
        let lifecycle = ResourceLifecycle::Active;
        let seq = inner.next_seq();
        inner
            .plugins
            .insert((tenant_id, id), Stored::new(plugin.clone(), lifecycle, seq));
        inner.plugin_names.insert((tenant_id, name), id);
        // The stored view is returned to the caller (`inst-mr-11`).
        Ok(plugin.clone())
    }

    fn replace(&self, plugin: &Plugin) -> Result<Plugin, DomainError> {
        validate_plugin_aggregate(plugin)?;
        let tenant_id = tenant_scope(plugin.tenant_id)?;
        let id = plugin.id.unwrap_or_default();
        let name = plugin.name.clone().unwrap_or_default();
        let mut inner = self.inner.write();
        if lookup(&inner.plugins, tenant_id, id, "plugin").is_err() {
            return Err(not_found("plugin"));
        }
        if let Some(holder) = inner.plugin_names.get(&(tenant_id, name.clone()))
            && *holder != id
        {
            return Err(already_exists("name", "name"));
        }
        if let Some(removed) = inner.plugins.remove(&(tenant_id, id))
            && let Some(stale) = removed.aggregate.name
            && Some(&stale) != Some(&name)
        {
            inner.plugin_names.remove(&(tenant_id, stale));
        }
        let seq = inner.next_seq();
        inner.plugins.insert(
            (tenant_id, id),
            Stored::new(plugin.clone(), ResourceLifecycle::Active, seq),
        );
        inner.plugin_names.insert((tenant_id, name), id);
        Ok(plugin.clone())
    }

    fn find(&self, tenant_id: Uuid, id: Uuid) -> Result<Plugin, DomainError> {
        let inner = self.inner.read();
        lookup(&inner.plugins, tenant_id, id, "plugin").map(|stored| stored.aggregate.clone())
    }

    fn find_by_name(&self, tenant_id: Uuid, name: &str) -> Result<Plugin, DomainError> {
        let inner = self.inner.read();
        let Some(id) = inner.plugin_names.get(&(tenant_id, name.to_owned())) else {
            return Err(DomainError::not_found(
                "name",
                format!("no plugin with the name '{name}' exists in the caller's tenant"),
            ));
        };
        lookup(&inner.plugins, tenant_id, *id, "plugin").map(|stored| stored.aggregate.clone())
    }

    fn list(&self, tenant_id: Uuid) -> Result<Vec<Plugin>, DomainError> {
        let inner = self.inner.read();
        let mut stored: Vec<&Stored<Plugin>> = inner
            .plugins
            .iter()
            .filter(|((tenant, _), _)| *tenant == tenant_id)
            .map(|(_, stored)| stored)
            .collect();
        stored.sort_by_key(|entry| entry.seq);
        Ok(stored
            .into_iter()
            .map(|entry| entry.aggregate.clone())
            .collect())
    }

    fn lifecycle(&self, tenant_id: Uuid, id: Uuid) -> Result<ResourceLifecycle, DomainError> {
        let inner = self.inner.read();
        if let Some(lifecycle) = inner.deleted.get(&(ResourceKind::Plugin, tenant_id, id)) {
            return Ok(*lifecycle);
        }
        lookup(&inner.plugins, tenant_id, id, "plugin").map(|stored| stored.lifecycle)
    }

    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<Plugin, DomainError> {
        let mut inner = self.inner.write();
        if lookup(&inner.plugins, tenant_id, id, "plugin").is_err() {
            return Err(not_found("plugin"));
        }
        // @cpt-begin:cpt-cf-oagw-algo-inmemory-repository:p1:inst-mr-12
        // The delete cascade of `cpt-cf-oagw-db-schema`: removing a plugin
        // removes any binding that still references it, whether a
        // still-referenced plugin may be deleted at all is decided by the
        // caller surface. The cascade is applied under the same lock as the
        // delete, so no caller observes a half-cascaded store.
        let reference = |binding: &Option<crate::domain::model::PluginsConfig>| {
            binding.as_ref().is_some_and(|plugins| {
                plugins
                    .bindings()
                    .iter()
                    .any(|binding| binding.plugin_uuid == Some(id))
            })
        };
        for ((upstream_tenant, _), stored) in inner.upstreams.iter_mut() {
            if *upstream_tenant != tenant_id || !reference(&stored.aggregate.plugins) {
                continue;
            }
            let plugins = stored.aggregate.plugins.as_mut().expect("referenced");
            plugins
                .items
                .retain(|item| crate::domain::model::plugin_ref_uuid(item) != Some(id));
        }
        for ((route_tenant, _), stored) in inner.routes.iter_mut() {
            if *route_tenant != tenant_id || !reference(&stored.aggregate.plugins) {
                continue;
            }
            let plugins = stored.aggregate.plugins.as_mut().expect("referenced");
            plugins
                .items
                .retain(|item| crate::domain::model::plugin_ref_uuid(item) != Some(id));
        }
        // @cpt-end:cpt-cf-oagw-algo-inmemory-repository:p1:inst-mr-12
        let removed = inner.plugins.remove(&(tenant_id, id));
        inner
            .plugin_names
            .retain(|name_key, holder| !(name_key.0 == tenant_id && *holder == id));
        inner.deleted.insert(
            (ResourceKind::Plugin, tenant_id, id),
            ResourceLifecycle::Deleted,
        );
        removed
            .map(|stored| stored.aggregate)
            .ok_or_else(|| not_found("plugin"))
    }

    fn delete_plugin_unreferenced(
        &self,
        tenant_id: Uuid,
        plugin_id: Uuid,
    ) -> Result<(), Vec<PluginReference>> {
        // The reference scan and the removal are one step under the store's
        // write lock, so no caller observes a plugin removed with a binding
        // left behind, or a binding added to a plugin already judged
        // unreferenced.
        let mut inner = self.inner.write();
        if !inner.plugins.contains_key(&(tenant_id, plugin_id)) {
            return Err(Vec::new());
        }
        let mut references: Vec<PluginReference> = Vec::new();
        for (key, stored) in inner.upstreams.iter() {
            if key.0 != tenant_id {
                continue;
            }
            for binding in stored
                .aggregate
                .plugins
                .iter()
                .flat_map(PluginsConfig::bindings)
            {
                if binding.plugin_uuid == Some(plugin_id) {
                    references.push(PluginReference {
                        kind: ResourceKind::Upstream,
                        id: key.1,
                        binding_name: binding.plugin_ref.clone(),
                    });
                }
            }
        }
        for (key, stored) in inner.routes.iter() {
            if key.0 != tenant_id {
                continue;
            }
            for binding in stored
                .aggregate
                .plugins
                .iter()
                .flat_map(PluginsConfig::bindings)
            {
                if binding.plugin_uuid == Some(plugin_id) {
                    references.push(PluginReference {
                        kind: ResourceKind::Route,
                        id: key.1,
                        binding_name: binding.plugin_ref.clone(),
                    });
                }
            }
        }
        if !references.is_empty() {
            return Err(references);
        }
        inner.plugins.remove(&(tenant_id, plugin_id));
        inner
            .plugin_names
            .retain(|(owner, _), id| !(owner == &tenant_id && id == &plugin_id));
        inner.deleted.insert(
            (ResourceKind::Plugin, tenant_id, plugin_id),
            ResourceLifecycle::Deleted,
        );
        Ok(())
    }
}

// @cpt-end:cpt-cf-oagw-dod-inmemory-repos:p1:inst-full
