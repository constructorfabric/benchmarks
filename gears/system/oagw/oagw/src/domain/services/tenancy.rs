//! Tenant hierarchy port.
//!
//! Alias shadowing and configuration inheritance both need the chain from a
//! tenant to its root. OAGW does not own the hierarchy — the tenant-resolver
//! gear does — so the domain depends only on this narrow trait.

use async_trait::async_trait;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::DomainResult;

/// Read-only view of the tenant tree.
#[async_trait]
pub trait TenantHierarchy: Send + Sync {
    /// Ancestor chain of `tenant_id`, **direct parent first, root last**.
    ///
    /// The tenant itself is not included. An unknown tenant yields an empty
    /// chain rather than an error: a caller whose tenant the resolver cannot
    /// see simply has no ancestors to inherit from.
    ///
    /// # Errors
    ///
    /// Propagates transport failures from the tenant resolver.
    async fn ancestors(&self, ctx: &SecurityContext, tenant_id: Uuid) -> DomainResult<Vec<Uuid>>;
}

/// A hierarchy with no ancestors — every tenant is its own root.
///
/// Used when the tenant-resolver client is unavailable, and by unit tests
/// that do not exercise inheritance.
#[derive(Debug, Default, Clone, Copy)]
pub struct FlatHierarchy;

#[async_trait]
impl TenantHierarchy for FlatHierarchy {
    async fn ancestors(&self, _ctx: &SecurityContext, _tenant_id: Uuid) -> DomainResult<Vec<Uuid>> {
        Ok(Vec::new())
    }
}

/// The resolution chain for `tenant_id`: the tenant itself followed by its
/// ancestors, i.e. **descendant → root**, which is the order alias shadowing
/// searches in.
///
/// # Errors
///
/// Propagates transport failures from the tenant resolver.
pub async fn resolution_chain(
    hierarchy: &dyn TenantHierarchy,
    ctx: &SecurityContext,
    tenant_id: Uuid,
) -> DomainResult<Vec<Uuid>> {
    let mut chain = vec![tenant_id];
    for ancestor in hierarchy.ancestors(ctx, tenant_id).await? {
        if !chain.contains(&ancestor) {
            chain.push(ancestor);
        }
    }
    Ok(chain)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Chain(Vec<Uuid>);

    #[async_trait]
    impl TenantHierarchy for Chain {
        async fn ancestors(
            &self,
            _ctx: &SecurityContext,
            _tenant_id: Uuid,
        ) -> DomainResult<Vec<Uuid>> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn chain_is_descendant_first() {
        let leaf = Uuid::new_v4();
        let parent = Uuid::new_v4();
        let root = Uuid::new_v4();
        let hierarchy = Chain(vec![parent, root]);
        let chain = resolution_chain(&hierarchy, &SecurityContext::anonymous(), leaf)
            .await
            .expect("chain");
        assert_eq!(chain, vec![leaf, parent, root]);
    }

    #[tokio::test]
    async fn flat_hierarchy_yields_only_the_tenant() {
        let leaf = Uuid::new_v4();
        let chain = resolution_chain(&FlatHierarchy, &SecurityContext::anonymous(), leaf)
            .await
            .expect("chain");
        assert_eq!(chain, vec![leaf]);
    }

    #[tokio::test]
    async fn a_cycle_cannot_duplicate_entries() {
        let leaf = Uuid::new_v4();
        let hierarchy = Chain(vec![leaf, leaf]);
        let chain = resolution_chain(&hierarchy, &SecurityContext::anonymous(), leaf)
            .await
            .expect("chain");
        assert_eq!(chain, vec![leaf]);
    }
}
