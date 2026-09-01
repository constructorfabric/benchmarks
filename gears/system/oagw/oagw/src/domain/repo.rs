//! Repository traits for the OAGW control plane.
//!
//! All repositories are tenant-scoped, mirroring the DB uniqueness
//! constraints from DESIGN §3.6:
//! - `oagw_upstream`: `UNIQUE (tenant_id, alias)`
//! - `oagw_route`: FK on `upstream_id` (cascade delete)
//! - `oagw_plugin`: `UNIQUE (tenant_id, name)`

use uuid::Uuid;

use super::dto::{Plugin, Route, Upstream};

/// Replacement / creation result for uniqueness conflicts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoConflict {
    /// `(tenant_id, alias)` already taken by another upstream.
    DuplicateAlias,
    /// `(tenant_id, name)` already taken by another plugin.
    DuplicateName,
    None,
}

/// Tenant-scoped upstream repository.
pub trait UpstreamRepository: Send + Sync {
    /// Insert a new upstream (id and `tenant_id` must already be assigned).
    ///
    /// # Errors
    /// Returns `RepoConflict::DuplicateAlias` on `(tenant_id, alias)` collision.
    fn insert(&self, upstream: Upstream) -> Result<(), RepoConflict>;
    /// Get an upstream by tenant + id.
    fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<Upstream>;
    /// Get an upstream by tenant + alias (case-insensitive normalization
    /// handled by the service).
    fn get_by_alias(&self, tenant_id: Uuid, alias: &str) -> Option<Upstream>;
    /// List all upstreams owned by a tenant.
    fn list(&self, tenant_id: Uuid) -> Vec<Upstream>;
    /// Replace an existing upstream (same id + tenant). Idempotent when the
    /// row does not exist — no-op.
    fn replace(&self, upstream: Upstream);
    /// Delete an upstream owned by a tenant. Returns `true` when deleted.
    fn delete(&self, tenant_id: Uuid, id: Uuid) -> bool;
}

/// Tenant-scoped route repository.
pub trait RouteRepository: Send + Sync {
    /// Insert a new route.
    ///
    /// # Errors
    /// Returns `RepoConflict::DuplicateAlias` when an enabled route with the
    /// same `(upstream_id, path, method)` already exists.
    fn insert(&self, route: Route) -> Result<(), RepoConflict>;
    /// Get a route by tenant + id.
    fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<Route>;
    /// List all routes owned by a tenant, optionally filtered by upstream.
    fn list(&self, tenant_id: Uuid, upstream_id: Option<Uuid>) -> Vec<Route>;
    /// Replace an existing route (same id + tenant). No-op when absent.
    fn replace(&self, route: Route);
    /// Delete a route owned by a tenant. Returns `true` when deleted.
    fn delete(&self, tenant_id: Uuid, id: Uuid) -> bool;
    /// Delete every route owned by a tenant that references `upstream_id`
    /// (cascade on upstream delete).
    fn delete_by_upstream(&self, tenant_id: Uuid, upstream_id: Uuid);
}

/// Tenant-scoped custom plugin repository.
pub trait PluginRepository: Send + Sync {
    /// Insert a new custom plugin.
    ///
    /// # Errors
    /// Returns `RepoConflict::DuplicateName` on `(tenant_id, name)` collision.
    fn insert(&self, plugin: Plugin) -> Result<(), RepoConflict>;
    /// Get a plugin by tenant + id.
    fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<Plugin>;
    /// List all plugins owned by a tenant.
    fn list(&self, tenant_id: Uuid) -> Vec<Plugin>;
    /// Delete a plugin owned by a tenant. Returns `true` when deleted.
    fn delete(&self, tenant_id: Uuid, id: Uuid) -> bool;
}
