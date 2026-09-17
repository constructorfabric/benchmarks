//! [`PluginRepository`] contract (mirrors `oagw_plugin` and the plugin
//! identification model).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::entity::{Plugin, PluginType, ReferencedBy};
use crate::domain::repo::RepoResult;

/// A catalog entry for plugin identification/query purposes.
///
/// Named (built-in) plugins are catalog-only — they resolve via in-process
/// registries and have no `oagw_plugin` row; custom plugins are UUID-backed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogEntry {
    /// Canonical GTS plugin identifier (e.g.
    /// `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1`).
    pub plugin_ref: String,
    /// UUID for custom plugins; `None` for catalog-only named plugins.
    pub plugin_uuid: Option<Uuid>,
    pub plugin_type: Option<PluginType>,
    /// Plugin name (`None` for catalog-only entries without a row).
    pub name: Option<String>,
}

/// Repository / registry for tenant-scoped custom plugins.
///
/// Mirrors `oagw_plugin`: PK `id`, UNIQUE `(tenant_id, name)`, immutable
/// after creation.  Also provides the catalog/source queries and the
/// in-use guard (409 `plugin.in_use`) used by deletion.
#[async_trait]
pub trait PluginRepository: Send + Sync {
    /// Persists a new custom plugin; rejects a duplicate `(tenant_id, name)`.
    ///
    /// # Errors
    /// - [`crate::domain::error::DomainError::Validation`] when the name is
    ///   already taken within the tenant.
    async fn create(&self, tenant_id: Uuid, plugin: Plugin) -> RepoResult<Plugin>;

    /// Fetches a custom plugin by `(tenant_id, id)`.
    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<Plugin>;

    /// Finds a custom plugin by `(tenant_id, name)`.
    async fn find_by_name(&self, tenant_id: Uuid, name: &str) -> Option<Plugin>;

    /// Lists all custom plugins owned by the tenant.
    async fn list(&self, tenant_id: Uuid) -> Vec<Plugin>;

    /// Returns the Starlark `source_code` of a custom plugin
    /// (`GET /plugins/{id}/source`).
    async fn source(&self, tenant_id: Uuid, id: Uuid) -> Option<String>;

    /// Returns the upstream/route bindings referencing the plugin by UUID —
    /// the `referenced_by` shape of the 409 `plugin.in_use` envelope (DoD
    /// `cpt-cf-oagw-dod-control-plane-api-plugin-crud`; ADR
    /// `cpt-cf-oagw-adr-request-routing`, "Plugin Deletion Behavior").
    async fn references(&self, plugin_id: Uuid) -> Vec<ReferencedBy>;

    /// Deletes a custom plugin by `(tenant_id, id)`.
    ///
    /// Returns `false` when no row was present.
    ///
    /// # Errors
    /// - [`crate::domain::error::DomainError::PluginInUse`] when upstream or
    ///   route bindings still reference the plugin.
    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> RepoResult<bool>;

    /// Catalog query: resolves whether a plugin reference names a UUID-backed
    /// custom plugin in this tenant, returning a [`CatalogEntry`] when it does.
    async fn catalog_entry(&self, tenant_id: Uuid, plugin_ref: &str) -> Option<CatalogEntry>;
}
