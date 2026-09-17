//! [`UpstreamRepository`] contract (mirrors `oagw_upstream`).

use async_trait::async_trait;
use uuid::Uuid;

use crate::domain::entity::Upstream;
use crate::domain::repo::RepoResult;

/// Repository for tenant-scoped `Upstream` entities.
///
/// Mirrors `oagw_upstream`: PK `id`, UNIQUE `(tenant_id, alias)`, `alias`
/// immutable once set.  All operations are tenant-scoped; `(tenant_id, alias)`
/// uniqueness and the configuration-boundary guards (alias pattern, SSRF
/// endpoint validation) are enforced before persistence so nothing invalid is
/// ever stored.
#[async_trait]
pub trait UpstreamRepository: Send + Sync {
    /// Persists a new upstream, assigning server-generated timestamps and
    /// rejecting a duplicate `(tenant_id, alias)` or an invalid alias/endpoint
    /// (alias pattern, SSRF guard).
    ///
    /// # Errors
    /// - [`crate::domain::error::DomainError::Validation`] when the alias is
    ///   invalid, an endpoint fails the SSRF guard, or `(tenant_id, alias)`
    ///   already exists.
    async fn create(&self, tenant_id: Uuid, upstream: Upstream) -> RepoResult<Upstream>;

    /// Fetches an upstream by `(tenant_id, id)`.
    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<Upstream>;

    /// Finds the upstream with the given `(tenant_id, alias)`.
    async fn find_by_alias(&self, tenant_id: Uuid, alias: &str) -> Option<Upstream>;

    /// Lists all upstreams owned by the tenant.
    async fn list(&self, tenant_id: Uuid) -> Vec<Upstream>;

    /// Replaces the stored upstream with `upstream` (same `id`), re-running
    /// the configuration-boundary guards and `(tenant_id, alias)` uniqueness.
    ///
    /// Returns `None` when no row with `(tenant_id, id)` exists.
    ///
    /// # Errors
    /// - [`crate::domain::error::DomainError::Validation`] as for
    ///   [`Self::create`].
    async fn update(&self, tenant_id: Uuid, upstream: Upstream) -> RepoResult<Option<Upstream>>;

    /// Deletes an upstream by `(tenant_id, id)`, cascading to its routes
    /// (mirrors the `oagw_route` FK `ON DELETE CASCADE`).
    ///
    /// Returns `false` when no row was present.
    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> RepoResult<bool>;
}
