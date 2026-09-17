//! DashMap-backed [`PluginRepository`] implementation.

use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use parking_lot::Mutex;
use uuid::Uuid;

use crate::domain::entity::{Plugin, ReferencedBy, ReferencedByResource};
use crate::domain::error::DomainError;
use crate::domain::repo::{CatalogEntry, PluginRepository, RepoResult};

use super::StoreTables;

/// In-memory `oagw_plugin` implementation (also the plugin registry).
pub struct InMemoryPluginRepo {
    tables: Arc<StoreTables>,
    lock: Arc<Mutex<()>>,
}

impl InMemoryPluginRepo {
    #[must_use]
    pub(crate) fn new(tables: Arc<StoreTables>, lock: Arc<Mutex<()>>) -> Self {
        Self { tables, lock }
    }

    /// Scans every upstream/route binding row for `plugin_uuid == id`,
    /// returning the referencing `(resource, id)` pairs.
    fn referenced_by_bindings(&self, plugin_id: Uuid) -> Vec<ReferencedBy> {
        let mut refs: Vec<ReferencedBy> = Vec::new();
        for r in self.tables.upstreams.iter() {
            let bound = r
                .value()
                .plugins
                .items
                .iter()
                .any(|b| b.plugin_uuid == Some(plugin_id));
            if bound {
                refs.push(ReferencedBy {
                    resource: ReferencedByResource::Upstream,
                    id: r.key().1,
                });
            }
        }
        for r in self.tables.routes.iter() {
            let bound = r
                .value()
                .plugins
                .items
                .iter()
                .any(|b| b.plugin_uuid == Some(plugin_id));
            if bound {
                refs.push(ReferencedBy {
                    resource: ReferencedByResource::Route,
                    id: r.key().1,
                });
            }
        }
        refs
    }
}

#[async_trait]
impl PluginRepository for InMemoryPluginRepo {
    async fn create(&self, tenant_id: Uuid, plugin: Plugin) -> RepoResult<Plugin> {
        let _guard = self.lock.lock();
        let name_key = (tenant_id, plugin.name.clone());
        if self.tables.plugin_name.contains_key(&name_key) {
            return Err(DomainError::validation(
                None,
                format!(
                    "plugin name '{}' already exists within the tenant",
                    plugin.name
                ),
            ));
        }

        let mut stored = plugin;
        let now = SystemTime::now();
        stored.created_at = Some(now);
        stored.updated_at = Some(now);

        self.tables.plugin_name.insert(name_key, stored.id);
        self.tables
            .plugins
            .insert((tenant_id, stored.id), stored.clone());
        Ok(stored)
    }

    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<Plugin> {
        self.tables.plugins.get(&(tenant_id, id)).map(|r| r.clone())
    }

    async fn find_by_name(&self, tenant_id: Uuid, name: &str) -> Option<Plugin> {
        let id = self.tables.plugin_name.get(&(tenant_id, name.to_owned()))?;
        self.tables
            .plugins
            .get(&(tenant_id, *id))
            .map(|r| r.clone())
    }

    async fn list(&self, tenant_id: Uuid) -> Vec<Plugin> {
        let mut rows: Vec<Plugin> = self
            .tables
            .plugins
            .iter()
            .filter(|r| r.key().0 == tenant_id)
            .map(|r| r.value().clone())
            .collect();
        rows.sort_by_key(|p| p.id);
        rows
    }

    async fn source(&self, tenant_id: Uuid, id: Uuid) -> Option<String> {
        self.tables
            .plugins
            .get(&(tenant_id, id))
            .map(|r| r.value().source_code.clone())
    }

    async fn references(&self, plugin_id: Uuid) -> Vec<ReferencedBy> {
        self.referenced_by_bindings(plugin_id)
    }

    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> RepoResult<bool> {
        let _guard = self.lock.lock();
        let key = (tenant_id, id);
        let Some(row) = self.tables.plugins.remove(&key) else {
            return Ok(false);
        };

        let referenced_by = self.referenced_by_bindings(id);
        if !referenced_by.is_empty() {
            // Re-insert and refuse: the plugin is still bound (409 plugin.in_use,
            // carrying the `referenced_by` shape per DoD
            // `cpt-cf-oagw-dod-control-plane-api-plugin-crud`).
            self.tables.plugins.insert(key, row.1.clone());
            return Err(DomainError::PluginInUse {
                name: row.1.name,
                referenced_by,
            });
        }

        self.tables.plugin_name.remove(&(tenant_id, row.1.name));
        Ok(true)
    }

    async fn catalog_entry(&self, tenant_id: Uuid, plugin_ref: &str) -> Option<CatalogEntry> {
        // Custom plugin references embed the plugin UUID after the '~' of the
        // instance-id form: `gts.cf.core.oagw.{type}_plugin.v1~{uuid}`.
        let uuid_part = plugin_ref.rsplit('~').next()?;
        let id = Uuid::parse_str(uuid_part).ok()?;
        let plugin = self.tables.plugins.get(&(tenant_id, id))?;
        Some(CatalogEntry {
            plugin_ref: plugin_ref.to_owned(),
            plugin_uuid: Some(plugin.id),
            plugin_type: Some(plugin.plugin_type),
            name: Some(plugin.name.clone()),
        })
    }
}
