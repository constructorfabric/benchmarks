//! Tenant hierarchy walks for hierarchical visibility (DESIGN.md §4.1).
//!
//! The control plane resolves a request's visibility set by walking the
//! tenant chain of the caller: the caller's own tenant first, then every
//! ancestor up to the root. Resources of *descendant* tenants are never
//! visible; a resource is reachable when its tenant is on that chain.
//!
//! The chain itself is supplied by [`TenantHierarchy`]: the production
//! implementation (see `crate::infra::hierarchy`) asks the `tenant-resolver`
//! gear, while tests use a static map.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::DomainError;

/// Supplier of a caller's tenant chain, ordered descendant → root.
#[async_trait]
pub trait TenantHierarchy: std::fmt::Debug + Send + Sync {
    /// Returns `tenant` followed by its ancestors, ordered descendant → root.
    ///
    /// # Errors
    /// Returns [`ErrorKind::TenantNotFound`] when the tenant does not exist
    /// and [`ErrorKind::Internal`] when the resolver is unavailable.
    async fn chain_from(
        &self,
        ctx: &SecurityContext,
        tenant: Uuid,
    ) -> Result<Vec<Uuid>, DomainError>;
}

/// Static hierarchy used by tests and single-tenant deployments.
///
/// The map holds `tenant → parent`; a missing entry means the tenant is a
/// root of its own chain, so an empty hierarchy gives every tenant a
/// single-element chain.
#[derive(Debug, Clone, Default)]
pub struct StaticTenantHierarchy {
    parents: Arc<HashMap<Uuid, Option<Uuid>>>,
}

impl StaticTenantHierarchy {
    /// Builds a hierarchy from `tenant → parent` edges; roots are omitted.
    #[must_use]
    pub fn new(parents: HashMap<Uuid, Uuid>) -> Self {
        let map = parents
            .into_iter()
            .map(|(tenant, parent)| (tenant, Some(parent)))
            .collect();
        Self {
            parents: Arc::new(map),
        }
    }

    /// A hierarchy with a single root tenant and no ancestors.
    #[must_use]
    pub fn single(tenant: Uuid) -> Self {
        let mut parents = HashMap::new();
        parents.insert(tenant, None);
        Self {
            parents: Arc::new(parents),
        }
    }

    /// Registers `tenant` as a root tenant.
    pub fn add_root(&mut self, tenant: Uuid) {
        Arc::make_mut(&mut self.parents).insert(tenant, None);
    }

    /// Registers `tenant → parent`, extending the chain.
    pub fn add(&mut self, tenant: Uuid, parent: Uuid) {
        Arc::make_mut(&mut self.parents).insert(tenant, Some(parent));
    }
}

#[async_trait]
impl TenantHierarchy for StaticTenantHierarchy {
    async fn chain_from(
        &self,
        _ctx: &SecurityContext,
        tenant: Uuid,
    ) -> Result<Vec<Uuid>, DomainError> {
        let mut chain = vec![tenant];
        let mut current = tenant;
        while let Some(Some(parent)) = self.parents.get(&current).copied() {
            chain.push(parent);
            current = parent;
        }
        Ok(chain)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> SecurityContext {
        SecurityContext::anonymous()
    }

    #[tokio::test]
    async fn walks_the_chain_descendant_to_root() {
        let root = Uuid::new_v4();
        let l1 = Uuid::new_v4();
        let leaf = Uuid::new_v4();
        let hierarchy = StaticTenantHierarchy::new(HashMap::from([(leaf, l1), (l1, root)]));
        let chain = hierarchy.chain_from(&context(), leaf).await.expect("chain");
        assert_eq!(chain, vec![leaf, l1, root]);

        let mut built = StaticTenantHierarchy::single(root);
        built.add(l1, root);
        built.add(leaf, l1);
        let chain = built.chain_from(&context(), leaf).await.expect("chain");
        assert_eq!(chain, vec![leaf, l1, root]);
    }

    #[tokio::test]
    async fn a_root_tenant_yields_a_single_element_chain() {
        let root = Uuid::new_v4();
        let mut hierarchy = StaticTenantHierarchy::single(root);
        hierarchy.add_root(root);
        let chain = hierarchy.chain_from(&context(), root).await.expect("chain");
        assert_eq!(chain, vec![root]);
    }

    #[tokio::test]
    async fn an_unregistered_tenant_is_its_own_root() {
        let hierarchy = StaticTenantHierarchy::default();
        let tenant = Uuid::new_v4();
        let chain = hierarchy
            .chain_from(&context(), tenant)
            .await
            .expect("chain");
        assert_eq!(chain, vec![tenant]);
    }
}
