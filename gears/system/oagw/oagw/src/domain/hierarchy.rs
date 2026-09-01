//! Tenant hierarchy traversal for the tenant-chain walk (alias shadowing /
//! resource inheritance).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;

use tenant_resolver_sdk::TenantResolverClient;
use toolkit_security::SecurityContext;

/// Resolves the tenant chain used when walking from a descendant up to the
/// root for alias resolution and hierarchical configuration merging.
#[async_trait]
pub trait TenantHierarchy: Send + Sync {
    /// Full chain for `tenant`, ordered ROOT → LEAF (leaf last), including
    /// `tenant` itself.  A single-tenant deployment yields `[tenant]`.
    async fn ancestor_chain(&self, ctx: &SecurityContext, tenant: Uuid) -> Vec<Uuid>;

    /// True when `ancestor` is reachable from `descendant` (or equal).
    async fn is_ancestor(&self, ctx: &SecurityContext, ancestor: Uuid, descendant: Uuid) -> bool;
}

/// Single-tenant / no-hierarchy mode: every tenant is its own root.
pub struct FlatTenantHierarchy;

#[async_trait]
impl TenantHierarchy for FlatTenantHierarchy {
    async fn ancestor_chain(&self, _ctx: &SecurityContext, tenant: Uuid) -> Vec<Uuid> {
        vec![tenant]
    }

    async fn is_ancestor(&self, _ctx: &SecurityContext, ancestor: Uuid, descendant: Uuid) -> bool {
        ancestor == descendant
    }
}

/// Static in-memory hierarchy (deterministic for tests / small deployments).
///
/// `parents: tenant -> parent` (missing = root).  Chains are walked to the
/// root; cycle-safety is enforced.
pub struct InMemoryHierarchy {
    parents: HashMap<Uuid, Uuid>,
}

impl InMemoryHierarchy {
    #[must_use]
    pub fn new(parents: HashMap<Uuid, Uuid>) -> Self {
        Self { parents }
    }

    fn chain(&self, tenant: Uuid) -> Vec<Uuid> {
        let mut chain = Vec::new();
        let mut cur = Some(tenant);
        let mut guard = 0_u8;
        while let Some(id) = cur {
            if guard > 64 {
                break; // cycle guard
            }
            guard += 1;
            chain.push(id);
            cur = self.parents.get(&id).copied();
        }
        chain.reverse(); // root → leaf
        chain
    }
}

#[async_trait]
impl TenantHierarchy for InMemoryHierarchy {
    async fn ancestor_chain(&self, _ctx: &SecurityContext, tenant: Uuid) -> Vec<Uuid> {
        self.chain(tenant)
    }

    async fn is_ancestor(&self, _ctx: &SecurityContext, ancestor: Uuid, descendant: Uuid) -> bool {
        self.chain(descendant).contains(&ancestor)
    }
}

/// Hierarchy resolved through the `TenantResolverClient` with a warm cache.
///
/// The resolver's `get_ancestors` is called lazily on cache miss (once per
/// tenant), then cached — proxy-time cost is a hash lookup.
pub struct ResolverHierarchy {
    resolver: Arc<dyn TenantResolverClient>,
    cache: dashmap::DashMap<Uuid, Vec<Uuid>>,
}

impl ResolverHierarchy {
    pub fn new(resolver: Arc<dyn TenantResolverClient>) -> Self {
        Self {
            resolver,
            cache: dashmap::DashMap::new(),
        }
    }
}

#[async_trait]
impl TenantHierarchy for ResolverHierarchy {
    async fn ancestor_chain(&self, ctx: &SecurityContext, tenant: Uuid) -> Vec<Uuid> {
        if let Some(chain) = self.cache.get(&tenant) {
            return chain.clone();
        }
        // Best-effort: the resolver may be unreachable — the tenant then stays
        // its own root (chain = [tenant]).
        let chain = if let Ok(resp) = self
            .resolver
            .get_ancestors(
                ctx,
                tenant_resolver_sdk::models::TenantId(tenant),
                &tenant_resolver_sdk::GetAncestorsOptions::default(),
            )
            .await
        {
            // resp.ancestors ordered direct parent → root.
            let mut ancestors: Vec<Uuid> =
                resp.ancestors.into_iter().map(|info| info.id.0).collect();
            ancestors.push(tenant);
            ancestors
        } else {
            vec![tenant]
        };
        self.cache.insert(tenant, chain.clone());
        chain
    }

    async fn is_ancestor(&self, ctx: &SecurityContext, ancestor: Uuid, descendant: Uuid) -> bool {
        self.ancestor_chain(ctx, descendant)
            .await
            .contains(&ancestor)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn ctx() -> SecurityContext {
        SecurityContext::anonymous()
    }

    fn uuid(n: u8) -> Uuid {
        Uuid::from_u128(u128::from(n))
    }

    #[tokio::test]
    async fn flat_chain_is_self() {
        let h = FlatTenantHierarchy;
        let chain = h.ancestor_chain(&ctx(), uuid(1)).await;
        assert_eq!(chain, vec![uuid(1)]);
        assert!(h.is_ancestor(&ctx(), uuid(1), uuid(1)).await);
        assert!(!h.is_ancestor(&ctx(), uuid(1), uuid(2)).await);
    }

    #[tokio::test]
    async fn in_memory_chain_orders_root_to_leaf() {
        // root(1) -> partner(2) -> leaf(3)
        let mut parents = HashMap::new();
        parents.insert(uuid(3), uuid(2));
        parents.insert(uuid(2), uuid(1));
        let h = InMemoryHierarchy::new(parents);
        assert_eq!(
            h.ancestor_chain(&ctx(), uuid(3)).await,
            vec![uuid(1), uuid(2), uuid(3)]
        );
        assert_eq!(h.ancestor_chain(&ctx(), uuid(1)).await, vec![uuid(1)]);
        assert!(h.is_ancestor(&ctx(), uuid(1), uuid(3)).await);
        assert!(!h.is_ancestor(&ctx(), uuid(3), uuid(1)).await);
    }
}
