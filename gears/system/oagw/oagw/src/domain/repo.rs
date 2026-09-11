//! Repository ports. The domain depends on these traits only; the in-memory
//! implementation lives in `infra`.

use async_trait::async_trait;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::{Plugin, Route, Upstream};

/// Port for upstream persistence.
#[async_trait]
pub trait UpstreamRepository: Send + Sync {
    /// Store a new upstream.
    ///
    /// # Errors
    /// Returns [`DomainError::Conflict`] when the `(tenant_id, alias)` pair is
    /// already taken.
    async fn insert(&self, upstream: Upstream) -> Result<(), DomainError>;

    /// Fetch an upstream owned by `tenant_id`.
    ///
    /// # Errors
    /// Never fails for the in-memory implementation.
    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Upstream>, DomainError>;

    /// Fetch an upstream by alias in a single tenant.
    ///
    /// # Errors
    /// Never fails for the in-memory implementation.
    async fn find_by_alias(
        &self,
        tenant_id: Uuid,
        alias: &str,
    ) -> Result<Option<Upstream>, DomainError>;

    /// All upstreams of one tenant.
    ///
    /// # Errors
    /// Never fails for the in-memory implementation.
    async fn list_by_tenant(&self, tenant_id: Uuid) -> Result<Vec<Upstream>, DomainError>;

    /// Every upstream, across tenants.
    ///
    /// # Errors
    /// Never fails for the in-memory implementation.
    async fn list_all(&self) -> Result<Vec<Upstream>, DomainError>;

    /// Overwrite an existing upstream.
    ///
    /// # Errors
    /// Returns [`DomainError::Conflict`] when the alias is taken by another
    /// upstream of the same tenant.
    async fn replace(&self, upstream: Upstream) -> Result<(), DomainError>;

    /// Remove an upstream. Returns `false` when it did not exist.
    ///
    /// # Errors
    /// Never fails for the in-memory implementation.
    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, DomainError>;
}

/// Port for route persistence.
#[async_trait]
pub trait RouteRepository: Send + Sync {
    /// Store a new route.
    ///
    /// # Errors
    /// Returns [`DomainError::Conflict`] on a duplicate match rule.
    async fn insert(&self, route: Route) -> Result<(), DomainError>;

    /// Fetch a route owned by `tenant_id`.
    ///
    /// # Errors
    /// Never fails for the in-memory implementation.
    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Route>, DomainError>;

    /// All routes of one tenant.
    ///
    /// # Errors
    /// Never fails for the in-memory implementation.
    async fn list_by_tenant(&self, tenant_id: Uuid) -> Result<Vec<Route>, DomainError>;

    /// Every route, across tenants.
    ///
    /// # Errors
    /// Never fails for the in-memory implementation.
    async fn list_all(&self) -> Result<Vec<Route>, DomainError>;

    /// Overwrite an existing route.
    ///
    /// # Errors
    /// Returns [`DomainError::Conflict`] on a duplicate match rule.
    async fn replace(&self, route: Route) -> Result<(), DomainError>;

    /// Delete a route. Returns `false` when it did not exist.
    ///
    /// # Errors
    /// Never fails for the in-memory implementation.
    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, DomainError>;
}

/// Port for plugin persistence.
#[async_trait]
pub trait PluginRepository: Send + Sync {
    /// Store a new plugin.
    ///
    /// # Errors
    /// Returns [`DomainError::Conflict`] on a duplicate `(tenant, name)`.
    async fn insert(&self, plugin: Plugin) -> Result<(), DomainError>;

    /// Fetch a plugin owned by `tenant_id`.
    ///
    /// # Errors
    /// Never fails for the in-memory implementation.
    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Plugin>, DomainError>;

    /// All plugins of one tenant.
    ///
    /// # Errors
    /// Never fails for the in-memory implementation.
    async fn list_by_tenant(&self, tenant_id: Uuid) -> Result<Vec<Plugin>, DomainError>;

    /// Delete a plugin. Returns `false` when it did not exist.
    ///
    /// # Errors
    /// Never fails for the in-memory implementation.
    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, DomainError>;
}
