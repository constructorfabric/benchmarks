//! [`TenantHierarchy`] backed by the tenant-resolver gear.

use async_trait::async_trait;
use std::sync::Arc;
use tenant_resolver_sdk::{GetAncestorsOptions, TenantId, TenantResolverClient};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::DomainResult;
use crate::domain::services::tenancy::TenantHierarchy;

/// Adapts `TenantResolverClient` to the narrow hierarchy port the domain
/// needs.
pub struct TenantResolverHierarchy {
    client: Arc<dyn TenantResolverClient>,
}

impl TenantResolverHierarchy {
    /// Wrap a resolver client.
    #[must_use]
    pub fn new(client: Arc<dyn TenantResolverClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl TenantHierarchy for TenantResolverHierarchy {
    async fn ancestors(&self, ctx: &SecurityContext, tenant_id: Uuid) -> DomainResult<Vec<Uuid>> {
        match self
            .client
            .get_ancestors(ctx, TenantId(tenant_id), &GetAncestorsOptions::default())
            .await
        {
            Ok(response) => Ok(response.ancestors.into_iter().map(|t| t.id.0).collect()),
            Err(err) => {
                // A caller whose tenant the resolver cannot see simply has no
                // ancestors to inherit from. Failing the proxy request here
                // would make an unrelated resolver hiccup look like a routing
                // error, so degrade to "no inheritance" and say so.
                tracing::warn!(
                    target: "oagw.tenancy",
                    %tenant_id,
                    error = %err,
                    "tenant ancestor lookup failed; resolving without inheritance"
                );
                Ok(Vec::new())
            }
        }
    }
}
