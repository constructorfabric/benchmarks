//! Persistence ports of the control plane.
//!
//! The domain speaks only to these traits; the in-memory implementation lives
//! in [`crate::infra::storage`], and a durable one replaces it without
//! touching the services.

use crate::domain::error::DomainError;
use crate::domain::model::{Plugin, PluginType, Route, Upstream};

/// Read/write access to the upstream table.
#[async_trait::async_trait]
pub trait UpstreamRepository: Send + Sync {
    /// Insert a new upstream. Errors with [`DomainError::Conflict`] when the
    /// tenant already routes the same alias.
    async fn insert(&self, upstream: &Upstream) -> Result<(), DomainError>;

    /// Replace an existing upstream wholesale.
    async fn update(&self, upstream: &Upstream) -> Result<(), DomainError>;

    /// Fetch one upstream of the tenant by id.
    async fn find(
        &self,
        tenant: uuid::Uuid,
        id: uuid::Uuid,
    ) -> Result<Option<Upstream>, DomainError>;

    /// Fetch one upstream of the tenant by its routing key.
    async fn find_by_alias(
        &self,
        tenant: uuid::Uuid,
        alias: &str,
    ) -> Result<Option<Upstream>, DomainError>;

    /// Every upstream of the tenant, ordered by alias.
    async fn list(&self, tenant: uuid::Uuid) -> Result<Vec<Upstream>, DomainError>;

    /// Remove one upstream of the tenant. `Ok(false)` when absent.
    async fn delete(&self, tenant: uuid::Uuid, id: uuid::Uuid) -> Result<bool, DomainError>;
}

/// Read/write access to the route table.
#[async_trait::async_trait]
pub trait RouteRepository: Send + Sync {
    /// Insert a new route.
    async fn insert(&self, route: &Route) -> Result<(), DomainError>;

    /// Replace an existing route wholesale.
    async fn update(&self, route: &Route) -> Result<(), DomainError>;

    /// Fetch one route of the tenant by id.
    async fn find(&self, tenant: uuid::Uuid, id: uuid::Uuid) -> Result<Option<Route>, DomainError>;

    /// Every route of the tenant, ordered by upstream then priority.
    async fn list(&self, tenant: uuid::Uuid) -> Result<Vec<Route>, DomainError>;

    /// Every route that targets one upstream, ordered by priority descending.
    async fn list_by_upstream(
        &self,
        tenant: uuid::Uuid,
        upstream: uuid::Uuid,
    ) -> Result<Vec<Route>, DomainError>;

    /// Remove a route. `Ok(false)` when absent.
    async fn delete(&self, tenant: uuid::Uuid, id: uuid::Uuid) -> Result<bool, DomainError>;
}

/// Read/write access to the custom plugin registry.
#[async_trait::async_trait]
pub trait PluginRepository: Send + Sync {
    /// Insert a new plugin row.
    async fn insert(&self, plugin: &Plugin) -> Result<(), DomainError>;

    /// Fetch one plugin of the tenant by id.
    async fn find(&self, tenant: uuid::Uuid, id: uuid::Uuid)
    -> Result<Option<Plugin>, DomainError>;

    /// Every plugin of the tenant, optionally narrowed to a plugin kind,
    /// ordered by name.
    async fn list(
        &self,
        tenant: uuid::Uuid,
        kind: Option<PluginType>,
    ) -> Result<Vec<Plugin>, DomainError>;

    /// Every upstream and route that references the plugin.
    ///
    /// Returned as GTS ids so the `409` body can carry them verbatim.
    async fn references(
        &self,
        tenant: uuid::Uuid,
        id: uuid::Uuid,
    ) -> Result<(Vec<String>, Vec<String>), DomainError>;

    /// Remove a plugin. `Ok(false)` when absent.
    async fn delete(&self, tenant: uuid::Uuid, id: uuid::Uuid) -> Result<bool, DomainError>;
}

/// Tenant-level authorization of a management operation.
///
/// The gear is registered behind the platform's authenticated router, so the
/// caller is already identified; this port decides whether that identity may
/// act on a management resource of the tenant.
#[async_trait::async_trait]
pub trait ManagementAuthorizer: Send + Sync {
    /// Authorize `action` (`read`/`write`/`delete`) on `resource`.
    ///
    /// # Errors
    /// Returns [`DomainError::AccessDenied`] when the caller is not entitled.
    async fn authorize(
        &self,
        tenant: uuid::Uuid,
        subject: &str,
        resource: &str,
        action: &str,
    ) -> Result<(), DomainError>;
}

/// An authorizer that admits every authenticated caller.
///
/// Used by tests and by deployments that run the control plane without the
/// policy engine.
#[derive(Debug, Clone, Copy, Default)]
pub struct AllowAllAuthorizer;

#[async_trait::async_trait]
impl ManagementAuthorizer for AllowAllAuthorizer {
    async fn authorize(
        &self,
        _tenant: uuid::Uuid,
        _subject: &str,
        _resource: &str,
        _action: &str,
    ) -> Result<(), DomainError> {
        Ok(())
    }
}
