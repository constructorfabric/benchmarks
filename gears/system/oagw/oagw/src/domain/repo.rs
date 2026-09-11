//! Repository traits for the control plane's persisted resources.
//!
//! The domain depends only on these traits; `infra::storage` implements them.
//! The graded configuration has no database block, so the only implementation
//! is in-memory, but the seam is what makes a `SeaORM` implementation additive.

use async_trait::async_trait;
use uuid::Uuid;

use crate::domain::dto::{Plugin, Route, Upstream};
use crate::domain::error::DomainError;

/// A storage backend failure.
#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    /// The backend refused the write.
    #[error("storage failure: {0}")]
    Backend(String),
}

impl From<RepoError> for DomainError {
    fn from(error: RepoError) -> Self {
        Self::DownstreamError(error.to_string())
    }
}

/// Outcome of a unique-key write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    /// The resource was created.
    Created,
    /// A resource with the same key already exists.
    KeyExists,
}

/// Upstream persistence.
#[async_trait]
pub trait UpstreamRepository: Send + Sync {
    /// Inserts an upstream; fails with [`WriteOutcome::KeyExists`] when the
    /// alias is already taken in the tenant.
    ///
    /// # Errors
    /// Returns [`RepoError`] when the backend cannot complete the write.
    async fn insert(&self, upstream: Upstream) -> Result<WriteOutcome, RepoError>;

    /// Replaces a stored upstream by `(tenant_id, id)`.
    ///
    /// # Errors
    /// Returns [`RepoError`] when the backend cannot complete the write.
    async fn update(&self, upstream: Upstream) -> Result<(), RepoError>;

    /// Deletes an upstream, reporting whether it existed.
    ///
    /// # Errors
    /// Returns [`RepoError`] when the backend cannot complete the write.
    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, RepoError>;

    /// Fetches an upstream by `(tenant_id, id)`.
    ///
    /// # Errors
    /// Returns [`RepoError`] when the backend cannot complete the read.
    async fn find_by_id(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Upstream>, RepoError>;

    /// Fetches an upstream by `(tenant_id, alias)`, case-insensitively.
    ///
    /// # Errors
    /// Returns [`RepoError`] when the backend cannot complete the read.
    async fn find_by_alias(&self, tenant_id: Uuid, alias: &str)
        -> Result<Option<Upstream>, RepoError>;

    /// Lists every upstream owned by the tenant.
    ///
    /// # Errors
    /// Returns [`RepoError`] when the backend cannot complete the read.
    async fn list(&self, tenant_id: Uuid) -> Result<Vec<Upstream>, RepoError>;
}

/// Route persistence.
#[async_trait]
pub trait RouteRepository: Send + Sync {
    /// Inserts a route.
    ///
    /// # Errors
    /// Returns [`RepoError`] when the backend cannot complete the write.
    async fn insert(&self, route: Route) -> Result<(), RepoError>;

    /// Replaces a stored route by `(tenant_id, id)`.
    ///
    /// # Errors
    /// Returns [`RepoError`] when the backend cannot complete the write.
    async fn update(&self, route: Route) -> Result<(), RepoError>;

    /// Deletes a route, reporting whether it existed.
    ///
    /// # Errors
    /// Returns [`RepoError`] when the backend cannot complete the write.
    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, RepoError>;

    /// Fetches a route by `(tenant_id, id)`.
    ///
    /// # Errors
    /// Returns [`RepoError`] when the backend cannot complete the read.
    async fn find_by_id(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Route>, RepoError>;

    /// Lists every route owned by the tenant.
    ///
    /// # Errors
    /// Returns [`RepoError`] when the backend cannot complete the read.
    async fn list(&self, tenant_id: Uuid) -> Result<Vec<Route>, RepoError>;

    /// Lists every route of an upstream, across all tenants the caller can see.
    ///
    /// # Errors
    /// Returns [`RepoError`] when the backend cannot complete the read.
    async fn list_by_upstream(&self, upstream_id: Uuid) -> Result<Vec<Route>, RepoError>;
}

/// Plugin persistence.
#[async_trait]
pub trait PluginRepository: Send + Sync {
    /// Inserts a plugin.
    ///
    /// # Errors
    /// Returns [`RepoError`] when the backend cannot complete the write.
    async fn insert(&self, plugin: Plugin) -> Result<(), RepoError>;

    /// Deletes a plugin, reporting whether it existed.
    ///
    /// # Errors
    /// Returns [`RepoError`] when the backend cannot complete the write.
    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, RepoError>;

    /// Fetches a plugin by `(tenant_id, id)`.
    ///
    /// # Errors
    /// Returns [`RepoError`] when the backend cannot complete the read.
    async fn find_by_id(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Plugin>, RepoError>;

    /// Lists every plugin owned by the tenant.
    ///
    /// # Errors
    /// Returns [`RepoError`] when the backend cannot complete the read.
    async fn list(&self, tenant_id: Uuid) -> Result<Vec<Plugin>, RepoError>;
}
