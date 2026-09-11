// Created: 2026-09-01 by Constructor Tech
//! Tenant-chain resolution against the `tenant_resolver` gear.
//!
//! `docs/DESIGN.md` §3.3 "Tenant Scoping": the management API is scoped to
//! the calling tenant only, while the data plane walks the ancestor chain
//! (descendant → root) to resolve an alias.

use std::sync::Arc;

use tenant_resolver_sdk::{GetAncestorsOptions, TenantId, TenantResolverClient};
use toolkit_security::SecurityContext;

/// Resolves the ancestor chain for a calling tenant.
#[derive(Clone)]
pub struct TenantChain {
    client: Option<Arc<dyn TenantResolverClient>>,
}

impl TenantChain {
    /// A chain with no backing resolver: the caller's tenant is the whole
    /// hierarchy.
    #[must_use]
    pub fn unlinked() -> Self {
        Self { client: None }
    }

    /// A chain backed by `client`.
    #[must_use]
    pub fn new(client: Arc<dyn TenantResolverClient>) -> Self {
        Self {
            client: Some(client),
        }
    }

    /// The ancestor chain for `tenant_id`, ordered descendant → root.
    ///
    /// # Errors
    /// Returns the resolver's failure verbatim.
    pub async fn chain(
        &self,
        ctx: &SecurityContext,
        tenant_id: uuid::Uuid,
    ) -> Result<Vec<String>, tenant_resolver_sdk::TenantResolverError> {
        let Some(client) = &self.client else {
            return Ok(vec![tenant_id.to_string()]);
        };
        let response = client
            .get_ancestors(ctx, TenantId(tenant_id), &GetAncestorsOptions::default())
            .await?;
        let mut chain = Vec::with_capacity(response.ancestors.len() + 1);
        chain.push(tenant_id.to_string());
        for ancestor in response.ancestors {
            chain.push(ancestor.id.0.to_string());
        }
        Ok(chain)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_unlinked_chain_is_a_singleton() {
        let ctx = SecurityContext::anonymous();
        let id = uuid::Uuid::new_v4();
        assert_eq!(
            TenantChain::unlinked()
                .chain(&ctx, id)
                .await
                .expect("chain"),
            vec![id.to_string()]
        );
    }
}
