//! Repository traits for the Control Plane store (DESIGN §3.2 DDD-Light).
//!
//! The domain layer depends only on these traits; the in-memory implementation
//! lives in [`crate::infra::storage::memory`] and a SeaORM-backed one can be
//! swapped in without touching the services.

use async_trait::async_trait;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::{Plugin, Route, Upstream};

/// View over stored upstreams, scoped by tenant.
#[async_trait]
pub trait UpstreamRepository: Send + Sync {
    /// Persists a new upstream.
    ///
    /// # Errors
    /// Returns [`DomainError::Conflict`] when the alias is already taken.
    async fn insert(&self, upstream: Upstream) -> Result<(), DomainError>;

    /// Reads an upstream by id, scoped to the tenant.
    async fn get(&self, tenant_id: &str, id: Uuid) -> Result<Option<Upstream>, DomainError>;

    /// Reads an upstream by its (normalized) alias, scoped to the tenant.
    async fn get_by_alias(
        &self,
        tenant_id: &str,
        alias: &str,
    ) -> Result<Option<Upstream>, DomainError>;

    /// Lists every upstream owned by the tenant.
    async fn list(&self, tenant_id: &str) -> Result<Vec<Upstream>, DomainError>;

    /// Replaces a stored upstream.
    ///
    /// # Errors
    /// Returns [`DomainError::NotFound`] when the upstream does not exist.
    async fn update(&self, upstream: Upstream) -> Result<(), DomainError>;

    /// Deletes an upstream, returning `false` when it did not exist.
    async fn delete(&self, tenant_id: &str, id: Uuid) -> Result<bool, DomainError>;
}

/// View over stored routes, scoped by tenant.
#[async_trait]
pub trait RouteRepository: Send + Sync {
    /// Persists a new route.
    async fn insert(&self, route: Route) -> Result<(), DomainError>;

    /// Reads a route by id, scoped to the tenant.
    async fn get(&self, tenant_id: &str, id: Uuid) -> Result<Option<Route>, DomainError>;

    /// Lists every route owned by the tenant.
    async fn list(&self, tenant_id: &str) -> Result<Vec<Route>, DomainError>;

    /// Lists every route owned by the tenant for one upstream.
    async fn list_by_upstream(
        &self,
        tenant_id: &str,
        upstream_id: Uuid,
    ) -> Result<Vec<Route>, DomainError>;

    /// Replaces a stored route.
    ///
    /// # Errors
    /// Returns [`DomainError::NotFound`] when the route does not exist.
    async fn update(&self, route: Route) -> Result<(), DomainError>;

    /// Deletes a route, returning `false` when it did not exist.
    async fn delete(&self, tenant_id: &str, id: Uuid) -> Result<bool, DomainError>;

    /// Deletes every route bound to an upstream (cascade).
    async fn delete_by_upstream(
        &self,
        tenant_id: &str,
        upstream_id: Uuid,
    ) -> Result<u64, DomainError>;
}

/// View over stored custom plugin definitions.
#[async_trait]
pub trait PluginRepository: Send + Sync {
    /// Persists a new plugin definition.
    async fn insert(&self, plugin: Plugin) -> Result<(), DomainError>;

    /// Reads a plugin by id, scoped to the tenant.
    async fn get(&self, tenant_id: &str, id: Uuid) -> Result<Option<Plugin>, DomainError>;

    /// Lists every plugin owned by the tenant.
    async fn list(&self, tenant_id: &str) -> Result<Vec<Plugin>, DomainError>;

    /// Deletes a plugin, returning `false` when it did not exist.
    async fn delete(&self, tenant_id: &str, id: Uuid) -> Result<bool, DomainError>;
}

/// Tenant-hierarchy view used for alias shadowing.
#[async_trait]
pub trait TenantHierarchy: Send + Sync {
    /// The tenant itself followed by its ancestors, closest first.
    ///
    /// Implementations must always start with `tenant_id` and must terminate
    /// (implementations cap the walk defensively).
    async fn chain(&self, tenant_id: &str) -> Vec<String>;
}
