//! Repository traits for the OAGW Control Plane.
//!
//! The contract repositories implement is tenant-scoped and returns plain
//! domain objects; persistence is delegated to the infrastructure layer. The
//! in-memory implementation lives in `infra::storage`.

use crate::domain::error::DomainError;
use crate::domain::model::{Plugin, PluginType, Route, Upstream};

/// Tenant-scoped upstream repository.
pub trait UpstreamRepository: Send + Sync {
    /// Inserts a new upstream.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::Conflict`] when `(tenant_id, alias)` already
    /// exists.
    fn insert(&self, upstream: Upstream) -> Result<(), DomainError>;

    /// Fetches an upstream by tenant and id.
    fn get(&self, tenant_id: uuid::Uuid, id: uuid::Uuid) -> Option<Upstream>;

    /// Fetches an upstream by tenant and alias (normalized comparison).
    fn find_by_alias(&self, tenant_id: uuid::Uuid, alias: &str) -> Option<Upstream>;

    /// Lists every upstream owned by the tenant, oldest first.
    fn list(&self, tenant_id: uuid::Uuid) -> Vec<Upstream>;

    /// Replaces an upstream owned by the tenant.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::Validation`] when the upstream is missing.
    fn replace(&self, upstream: Upstream) -> Result<(), DomainError>;

    /// Deletes an upstream owned by the tenant. Returns `true` when deleted.
    fn delete(&self, tenant_id: uuid::Uuid, id: uuid::Uuid) -> bool;
}

/// Tenant-scoped route repository.
pub trait RouteRepository: Send + Sync {
    /// Inserts a new route.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::Validation`] when the upstream is unknown.
    fn insert(&self, route: Route) -> Result<(), DomainError>;

    /// Fetches a route by tenant and id.
    fn get(&self, tenant_id: uuid::Uuid, id: uuid::Uuid) -> Option<Route>;

    /// Fetches a route by id regardless of tenant (hierarchy lookups).
    fn get_any_tenant(&self, id: uuid::Uuid) -> Option<Route>;

    /// Lists every route owned by the tenant, oldest first.
    fn list(&self, tenant_id: uuid::Uuid) -> Vec<Route>;

    /// Lists every route for an upstream across all tenants.
    fn list_by_upstream(&self, upstream_id: uuid::Uuid) -> Vec<Route>;

    /// Replaces a route owned by the tenant.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::Validation`] when the route is missing.
    fn replace(&self, route: Route) -> Result<(), DomainError>;

    /// Deletes a route owned by the tenant. Returns `true` when deleted.
    fn delete(&self, tenant_id: uuid::Uuid, id: uuid::Uuid) -> bool;
}

/// Tenant-scoped custom (Starlark) plugin repository.
pub trait PluginRepository: Send + Sync {
    /// Inserts a plugin.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::Conflict`] when the `(tenant, name)` pair
    /// already exists.
    fn insert(&self, plugin: Plugin) -> Result<(), DomainError>;

    /// Fetches a plugin by id.
    fn get(&self, id: uuid::Uuid) -> Option<Plugin>;

    /// Lists every plugin of the tenant, optionally filtered by type.
    fn list(&self, tenant_id: uuid::Uuid, plugin_type: Option<PluginType>) -> Vec<Plugin>;

    /// Deletes a plugin. Returns `true` when it existed.
    fn delete(&self, id: uuid::Uuid) -> bool;

    /// Finds a plugin by tenant and name.
    fn find_by_name(&self, tenant_id: uuid::Uuid, name: &str) -> Option<Plugin>;
}
