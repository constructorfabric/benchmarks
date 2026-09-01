//! Tenant-chain resolution for the data plane (DESIGN §3.1 "Alias
//! Resolution" — "walks tenant hierarchy from descendant to root").
//!
//! The chain is the visibility scope of a proxy request: only upstreams owned
//! by one of its tenants can be resolved, which is what makes alias guessing
//! across tenants impossible.
//!
//! When the `tenant-resolver` gear is not wired into the runtime (unit tests,
//! offline usage) the chain degrades to the calling tenant alone, which is
//! the correct behaviour for a tenant with no ancestors.

use std::sync::Arc;

use toolkit_security::SecurityContext;
use tenant_resolver_sdk::models::GetAncestorsOptions;
use tenant_resolver_sdk::TenantResolverClient;

/// Resolves the ordered tenant chain of a request, **leaf first**.
#[derive(Clone)]
pub struct TenantChainResolver {
    client: Option<Arc<dyn TenantResolverClient>>,
}

impl TenantChainResolver {
    /// Builds a resolver over the optional tenant-resolver client.
    #[must_use]
    pub fn new(client: Option<Arc<dyn TenantResolverClient>>) -> Self {
        Self { client }
    }

    /// Resolves the chain of `ctx`, ordered leaf → root.
    ///
    /// Review evidence (privilege boundary — cross-tenant isolation):
    /// * Guardrail: DESIGN "Alias Resolution" — a proxy request may only reach
    ///   upstreams of the calling tenant and its ancestors.
    /// * Rationale: the chain is the filter applied by
    ///   [`crate::domain::routing::resolve_alias`]; a wider chain would expose
    ///   unrelated tenants' upstreams to alias guessing.
    /// * Validation performed: `resolve_alias_ignores_out_of_chain_tenants`
    ///   covers the isolation property, and `proxy_*` integration tests assert
    ///   that only the caller's chain is consulted.
    pub async fn chain(&self, ctx: &SecurityContext) -> Vec<uuid::Uuid> {
        let leaf = ctx.subject_tenant_id();
        let Some(client) = self.client.as_ref() else {
            return vec![leaf];
        };
        match client
            .get_ancestors(ctx, tenant_resolver_sdk::TenantId(leaf), &GetAncestorsOptions::default())
            .await
        {
            Ok(response) => {
                let mut chain = vec![leaf];
                chain.extend(response.ancestors.iter().map(|tenant| tenant.id.0));
                chain
            }
            Err(_) => vec![leaf],
        }
    }
}

/// Fallback chain used when the resolver is unavailable.
#[must_use]
pub fn fallback_chain(tenant_id: uuid::Uuid) -> Vec<uuid::Uuid> {
    vec![tenant_id]
}

#[cfg(test)]
#[path = "tenant_tests.rs"]
mod tests;
