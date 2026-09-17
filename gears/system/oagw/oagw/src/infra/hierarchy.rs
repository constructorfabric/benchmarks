//! `TenantHierarchy` backed by the `tenant-resolver` gear.

use std::sync::Arc;

use async_trait::async_trait;
use tenant_resolver_sdk::{
    GetAncestorsOptions, TenantId, TenantResolverClient, TenantResolverError,
};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::{DomainError, ErrorKind};
use crate::domain::hierarchy::TenantHierarchy;

/// Walks the tenant chain through the platform tenant resolver.
///
/// Ancestor traversal respects barriers (`BarrierMode::Respect`), so a
/// self-managed tenant stops the chain at itself.
pub struct ResolverTenantHierarchy {
    resolver: Arc<dyn TenantResolverClient>,
}

impl std::fmt::Debug for ResolverTenantHierarchy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolverTenantHierarchy")
            .finish_non_exhaustive()
    }
}

impl ResolverTenantHierarchy {
    /// Wraps `resolver`.
    #[must_use]
    pub fn new(resolver: Arc<dyn TenantResolverClient>) -> Self {
        Self { resolver }
    }
}

#[async_trait]
impl TenantHierarchy for ResolverTenantHierarchy {
    async fn chain_from(
        &self,
        ctx: &SecurityContext,
        tenant: Uuid,
    ) -> Result<Vec<Uuid>, DomainError> {
        let options = GetAncestorsOptions::default();
        let response = self
            .resolver
            .get_ancestors(ctx, TenantId(tenant), &options)
            .await
            .map_err(|error| match error {
                TenantResolverError::TenantNotFound { .. } => DomainError::new(
                    ErrorKind::TenantNotFound,
                    format!("tenant {tenant} is not known to the tenant resolver"),
                ),
                other => DomainError::new(
                    ErrorKind::Internal,
                    format!("tenant resolver unavailable: {other}"),
                ),
            })?;
        let mut chain = vec![tenant];
        chain.extend(response.ancestors.iter().map(|ancestor| ancestor.id.0));
        Ok(chain)
    }
}
