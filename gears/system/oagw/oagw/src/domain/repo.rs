//! Repository contracts (DESIGN §1.3 DDD-Light layering).
//!
//! The domain owns these traits; `infra::storage` provides the in-memory
//! implementation used by the MVP control plane (no `database:` block is
//! configured for the gear). The traits are deliberately coarse-grained: the
//! multi-table invariants (route → upstream, plugin references, cascade
//! delete) are implemented by [`crate::domain::service`] on top of them.

use async_trait::async_trait;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::models::{Plugin, Route, Upstream};

/// Persistence contract for the OAGW control plane.
///
/// Every operation is tenant-scoped: a caller may only ever observe rows whose
/// `tenant_id` matches the scope passed in (DESIGN §3.3 "Tenant Scoping").
#[async_trait]
pub trait ControlPlaneStore: Send + Sync {
    /// Persists a new upstream. Fails with `Conflict` when the id exists.
    ///
    /// # Errors
    ///
    /// Returns a [`DomainError`] on storage failure or duplicate id.
    async fn insert_upstream(&self, upstream: Upstream) -> Result<(), DomainError>;

    /// Loads an upstream by id and tenant.
    ///
    /// # Errors
    ///
    /// Returns a [`DomainError`] on storage failure.
    async fn get_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Upstream>, DomainError>;

    /// Loads an upstream by normalized alias and tenant.
    ///
    /// # Errors
    ///
    /// Returns a [`DomainError`] on storage failure.
    async fn find_upstream_by_alias(
        &self,
        tenant_id: Uuid,
        alias: &str,
    ) -> Result<Option<Upstream>, DomainError>;

    /// Lists every upstream of a tenant, unordered.
    ///
    /// # Errors
    ///
    /// Returns a [`DomainError`] on storage failure.
    async fn list_upstreams(&self, tenant_id: Uuid) -> Result<Vec<Upstream>, DomainError>;

    /// Replaces an existing upstream row.
    ///
    /// # Errors
    ///
    /// Returns a [`DomainError`] when the row does not exist.
    async fn update_upstream(&self, upstream: Upstream) -> Result<(), DomainError>;

    /// Removes an upstream, returning whether it existed.
    ///
    /// # Errors
    ///
    /// Returns a [`DomainError`] on storage failure.
    async fn delete_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, DomainError>;

    /// Persists a new route. Fails with `Conflict` when the id exists.
    ///
    /// # Errors
    ///
    /// Returns a [`DomainError`] on storage failure or duplicate id.
    async fn insert_route(&self, route: Route) -> Result<(), DomainError>;

    /// Loads a route by id and tenant.
    ///
    /// # Errors
    ///
    /// Returns a [`DomainError`] on storage failure.
    async fn get_route(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Route>, DomainError>;

    /// Lists every route of a tenant, unordered.
    ///
    /// # Errors
    ///
    /// Returns a [`DomainError`] on storage failure.
    async fn list_routes(&self, tenant_id: Uuid) -> Result<Vec<Route>, DomainError>;

    /// Lists the routes of one upstream.
    ///
    /// # Errors
    ///
    /// Returns a [`DomainError`] on storage failure.
    async fn list_routes_by_upstream(
        &self,
        tenant_id: Uuid,
        upstream_id: Uuid,
    ) -> Result<Vec<Route>, DomainError>;

    /// Replaces an existing route row.
    ///
    /// # Errors
    ///
    /// Returns a [`DomainError`] when the row does not exist.
    async fn update_route(&self, route: Route) -> Result<(), DomainError>;

    /// Removes a route, returning whether it existed.
    ///
    /// # Errors
    ///
    /// Returns a [`DomainError`] on storage failure.
    async fn delete_route(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, DomainError>;

    /// Persists a new plugin. Fails with `Conflict` when the id exists.
    ///
    /// # Errors
    ///
    /// Returns a [`DomainError`] on storage failure or duplicate id.
    async fn insert_plugin(&self, plugin: Plugin) -> Result<(), DomainError>;

    /// Loads a plugin by id and tenant.
    ///
    /// # Errors
    ///
    /// Returns a [`DomainError`] on storage failure.
    async fn get_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Plugin>, DomainError>;

    /// Loads a plugin by unique name and tenant.
    ///
    /// # Errors
    ///
    /// Returns a [`DomainError`] on storage failure.
    async fn find_plugin_by_name(
        &self,
        tenant_id: Uuid,
        name: &str,
    ) -> Result<Option<Plugin>, DomainError>;

    /// Lists every plugin of a tenant, unordered.
    ///
    /// # Errors
    ///
    /// Returns a [`DomainError`] on storage failure.
    async fn list_plugins(&self, tenant_id: Uuid) -> Result<Vec<Plugin>, DomainError>;

    /// Removes a plugin, returning whether it existed.
    ///
    /// # Errors
    ///
    /// Returns a [`DomainError`] on storage failure.
    async fn delete_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, DomainError>;
}
