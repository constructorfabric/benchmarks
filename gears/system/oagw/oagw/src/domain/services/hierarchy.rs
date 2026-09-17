//! Tenant-hierarchy abstraction used by the data plane for alias shadowing
//! and enforced-ancestor configuration.

use async_trait::async_trait;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::TenantId;

/// Resolves the tenant ancestry required by alias resolution.
///
/// The production implementation walks the tenant resolver; tests supply an
/// in-memory hierarchy.
#[async_trait]
pub trait TenantHierarchy: Send + Sync {
    /// Return the tenant chain from `tenant` up to (and including) the root,
    /// ordered descendant-first: `[tenant, parent, ..., root]`.
    ///
    /// A root tenant yields `[root]`.
    ///
    /// # Errors
    ///
    /// `DomainError::Internal` when the hierarchy cannot be resolved.
    async fn tenant_chain(&self, tenant: TenantId) -> Result<Vec<Uuid>, DomainError>;
}
