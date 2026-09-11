//! Repository ports.
//!
//! The Control Plane service depends only on these traits, so an in-memory
//! implementation can be replaced by a durable one without touching domain or
//! transport code.

use crate::domain::error::DomainError;
use crate::domain::model::{Plugin, Route, Upstream};
use async_trait::async_trait;
use uuid::Uuid;

/// Tenant-scoped persistence for upstreams.
#[async_trait]
pub trait UpstreamRepository: Send + Sync {
    /// Stores a new upstream; fails when the alias is already taken.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::Conflict`] when the alias is in use.
    async fn create(&self, upstream: Upstream) -> Result<Upstream, DomainError>;

    /// Reads an upstream by id.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::NotFound`] when absent.
    async fn get(&self, tenant_id: Uuid, id: &str) -> Result<Upstream, DomainError>;

    /// Reads an upstream by its normalised alias.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::NotFound`] when absent.
    async fn find_by_alias(&self, tenant_id: Uuid, alias: &str) -> Result<Upstream, DomainError>;

    /// Lists every upstream of the tenant.
    ///
    /// # Errors
    ///
    /// Never fails for the in-memory implementation.
    async fn list(&self, tenant_id: Uuid) -> Result<Vec<Upstream>, DomainError>;

    /// Replaces an existing upstream.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::NotFound`] when absent.
    async fn replace(&self, upstream: Upstream) -> Result<Upstream, DomainError>;

    /// Removes an upstream and every route under it.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::NotFound`] when absent.
    async fn delete(&self, tenant_id: Uuid, id: &str) -> Result<(), DomainError>;
}

/// Tenant-scoped persistence for routes.
#[async_trait]
pub trait RouteRepository: Send + Sync {
    /// Stores a new route; fails on a duplicate match rule.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::Conflict`] on a duplicate match rule.
    async fn create(&self, route: Route) -> Result<Route, DomainError>;

    /// Reads a route by id.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::NotFound`] when absent.
    async fn get(&self, tenant_id: Uuid, id: &str) -> Result<Route, DomainError>;

    /// Lists every route of the tenant.
    ///
    /// # Errors
    ///
    /// Never fails for the in-memory implementation.
    async fn list(&self, tenant_id: Uuid) -> Result<Vec<Route>, DomainError>;

    /// Lists every route referencing the given upstream.
    ///
    /// # Errors
    ///
    /// Never fails for the in-memory implementation.
    async fn list_by_upstream(
        &self,
        tenant_id: Uuid,
        upstream_id: &str,
    ) -> Result<Vec<Route>, DomainError>;

    /// Replaces an existing route.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::NotFound`] when absent.
    async fn replace(&self, route: Route) -> Result<Route, DomainError>;

    /// Removes a route.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::NotFound`] when absent.
    async fn delete(&self, tenant_id: Uuid, id: &str) -> Result<(), DomainError>;

    /// Removes every route under the given upstream (cascade delete).
    ///
    /// # Errors
    ///
    /// Never fails for the in-memory implementation.
    async fn delete_by_upstream(
        &self,
        tenant_id: Uuid,
        upstream_id: &str,
    ) -> Result<(), DomainError>;
}

/// Tenant-scoped persistence for custom plugins.
#[async_trait]
pub trait PluginRepository: Send + Sync {
    /// Stores a new plugin.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::Conflict`] when the name is in use.
    async fn create(&self, plugin: Plugin) -> Result<Plugin, DomainError>;

    /// Reads a plugin by id.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::NotFound`] when absent.
    async fn get(&self, tenant_id: Uuid, id: &str) -> Result<Plugin, DomainError>;

    /// Lists every plugin of the tenant.
    ///
    /// # Errors
    ///
    /// Never fails for the in-memory implementation.
    async fn list(&self, tenant_id: Uuid) -> Result<Vec<Plugin>, DomainError>;

    /// Removes an unreferenced plugin.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::NotFound`] when absent.
    async fn delete(&self, tenant_id: Uuid, id: &str) -> Result<(), DomainError>;
}
