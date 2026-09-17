//! Tenant-hierarchy adapter: `tenant_resolver_sdk` behind the domain port.
//!
//! The data plane only needs one thing from the tenant resolver — the ancestor
//! chain of the calling tenant — and must never widen it: the chain is what
//! decides whose upstreams a request may reach (DESIGN §3.3 "Tenant Scoping").

use async_trait::async_trait;
use tenant_resolver_sdk::{GetAncestorsOptions, TenantResolverClient};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::routing::TenantHierarchy;

/// `TenantResolverClient` adapter for [`TenantHierarchy`].
///
/// A resolution failure degrades to the calling tenant alone rather than
/// failing the request: the walk can only ever consult *fewer* tenants, so the
/// worst case is a 404 for an alias the caller would otherwise have inherited —
/// never access to another tenant's configuration. The failure is logged so the
/// degradation is visible.
pub struct TenantResolverHierarchy {
    client: std::sync::Arc<dyn TenantResolverClient>,
}

impl TenantResolverHierarchy {
    /// Wrap a resolver client.
    #[must_use]
    pub const fn new(client: std::sync::Arc<dyn TenantResolverClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl TenantHierarchy for TenantResolverHierarchy {
    async fn chain(&self, security: &SecurityContext, tenant: Uuid) -> Vec<Uuid> {
        match self
            .client
            .get_ancestors(
                security,
                tenant_resolver_sdk::TenantId(tenant),
                &GetAncestorsOptions::default(),
            )
            .await
        {
            Ok(response) => {
                let mut chain = Vec::with_capacity(response.ancestors.len() + 1);
                chain.push(tenant);
                // The resolver reports the chain parent → root; the proxy walks
                // nearest first, which is the same order.
                for ancestor in response.ancestors {
                    chain.push(ancestor.id.0);
                }
                chain
            }
            Err(error) => {
                tracing::warn!(
                    target: "oagw.proxy",
                    tenant_id = %tenant,
                    error = %error,
                    "tenant hierarchy unavailable; falling back to the calling tenant only"
                );
                vec![tenant]
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A resolver stub that returns a fixed chain, or fails.
    struct StubResolver {
        ancestors: Vec<tenant_resolver_sdk::TenantRef>,
        fail: bool,
    }

    #[async_trait]
    impl TenantResolverClient for StubResolver {
        async fn get_tenant(
            &self,
            _ctx: &SecurityContext,
            _id: tenant_resolver_sdk::TenantId,
        ) -> Result<tenant_resolver_sdk::TenantInfo, tenant_resolver_sdk::TenantResolverError>
        {
            unimplemented!("not needed for the hierarchy adapter");
        }

        async fn get_root_tenant(
            &self,
            _ctx: &SecurityContext,
        ) -> Result<tenant_resolver_sdk::TenantInfo, tenant_resolver_sdk::TenantResolverError>
        {
            unimplemented!("not needed for the hierarchy adapter");
        }

        async fn get_tenants(
            &self,
            _ctx: &SecurityContext,
            _ids: &[tenant_resolver_sdk::TenantId],
            _options: &tenant_resolver_sdk::GetTenantsOptions,
        ) -> Result<Vec<tenant_resolver_sdk::TenantInfo>, tenant_resolver_sdk::TenantResolverError>
        {
            unimplemented!("not needed for the hierarchy adapter");
        }

        async fn get_descendants(
            &self,
            _ctx: &SecurityContext,
            _id: tenant_resolver_sdk::TenantId,
            _options: &tenant_resolver_sdk::GetDescendantsOptions,
        ) -> Result<
            tenant_resolver_sdk::GetDescendantsResponse,
            tenant_resolver_sdk::TenantResolverError,
        > {
            unimplemented!("not needed for the hierarchy adapter");
        }

        async fn is_ancestor(
            &self,
            _ctx: &SecurityContext,
            _ancestor: tenant_resolver_sdk::TenantId,
            _descendant: tenant_resolver_sdk::TenantId,
            _options: &tenant_resolver_sdk::IsAncestorOptions,
        ) -> Result<bool, tenant_resolver_sdk::TenantResolverError> {
            unimplemented!("not needed for the hierarchy adapter");
        }

        async fn get_ancestors(
            &self,
            _ctx: &SecurityContext,
            _id: tenant_resolver_sdk::TenantId,
            _options: &GetAncestorsOptions,
        ) -> Result<
            tenant_resolver_sdk::GetAncestorsResponse,
            tenant_resolver_sdk::TenantResolverError,
        > {
            if self.fail {
                return Err(tenant_resolver_sdk::TenantResolverError::TenantNotFound {
                    tenant_id: tenant_resolver_sdk::TenantId(Uuid::nil()),
                });
            }

            Ok(tenant_resolver_sdk::GetAncestorsResponse {
                tenant: tenant_resolver_sdk::TenantRef {
                    id: tenant_resolver_sdk::TenantId(Uuid::nil()),
                    status: tenant_resolver_sdk::TenantStatus::Active,
                    tenant_type: None,
                    parent_id: None,
                    self_managed: false,
                },
                ancestors: self.ancestors.clone(),
            })
        }
    }

    #[tokio::test]
    async fn the_chain_starts_at_the_caller_and_walks_to_the_root() {
        let parent = Uuid::new_v4();
        let root = Uuid::new_v4();
        let tenant = Uuid::new_v4();

        let resolver = TenantResolverHierarchy::new(std::sync::Arc::new(StubResolver {
            ancestors: vec![
                tenant_resolver_sdk::TenantRef {
                    id: tenant_resolver_sdk::TenantId(parent),
                    status: tenant_resolver_sdk::TenantStatus::Active,
                    tenant_type: None,
                    parent_id: None,
                    self_managed: false,
                },
                tenant_resolver_sdk::TenantRef {
                    id: tenant_resolver_sdk::TenantId(root),
                    status: tenant_resolver_sdk::TenantStatus::Active,
                    tenant_type: None,
                    parent_id: None,
                    self_managed: false,
                },
            ],
            fail: false,
        }));

        let chain = resolver.chain(&SecurityContext::anonymous(), tenant).await;

        assert_eq!(chain, vec![tenant, parent, root]);
    }

    #[tokio::test]
    async fn a_resolution_failure_degrades_to_the_calling_tenant() {
        let tenant = Uuid::new_v4();
        let resolver = TenantResolverHierarchy::new(std::sync::Arc::new(StubResolver {
            ancestors: Vec::new(),
            fail: true,
        }));

        let chain = resolver.chain(&SecurityContext::anonymous(), tenant).await;

        assert_eq!(chain, vec![tenant]);
    }
}
