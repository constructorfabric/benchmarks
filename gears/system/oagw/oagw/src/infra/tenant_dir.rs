//! Tenant-hierarchy lookup.
//!
//! Alias resolution and configuration merging both walk the tenant chain from
//! the caller's tenant up to the root (`cpt-cf-oagw-fr-alias-resolution`).
//! OAGW does not own the hierarchy — the tenant-resolver gear does — so the
//! walk is behind a trait with a short-lived cache in front of it, keeping the
//! per-request cost off the hot path.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dashmap::DashMap;
use tenant_resolver_sdk::{GetAncestorsOptions, TenantId, TenantResolverClient};
use toolkit_security::SecurityContext;
use uuid::Uuid;

pub use crate::domain::tenant::TenantChain;

/// [`TenantChain`] backed by the tenant-resolver gear.
pub struct TenantResolverChain {
    client: Arc<dyn TenantResolverClient>,
    cache: DashMap<Uuid, (Instant, Vec<Uuid>)>,
    ttl: Duration,
}

impl TenantResolverChain {
    /// Wrap a tenant-resolver client with a `ttl`-second ancestor cache.
    #[must_use]
    pub fn new(client: Arc<dyn TenantResolverClient>, ttl_secs: u64) -> Self {
        Self {
            client,
            cache: DashMap::new(),
            ttl: Duration::from_secs(ttl_secs.max(1)),
        }
    }
}

#[async_trait]
impl TenantChain for TenantResolverChain {
    async fn ancestors(&self, ctx: &SecurityContext, tenant_id: Uuid) -> Vec<Uuid> {
        if let Some(entry) = self.cache.get(&tenant_id)
            && entry.value().0.elapsed() < self.ttl
        {
            return entry.value().1.clone();
        }
        let ancestors = match self
            .client
            .get_ancestors(ctx, TenantId(tenant_id), &GetAncestorsOptions::default())
            .await
        {
            Ok(response) => response
                .ancestors
                .into_iter()
                .map(|tenant| tenant.id.0)
                .collect::<Vec<_>>(),
            Err(err) => {
                // A hierarchy lookup failure must not fail the request: the
                // caller's own tenant is always in the chain, so the worst
                // case is that inherited configuration is briefly invisible.
                tracing::warn!(
                    target: "oagw.tenant",
                    %tenant_id,
                    error = %err,
                    "tenant ancestor lookup failed; proceeding without inherited configuration"
                );
                Vec::new()
            }
        };
        self.cache
            .insert(tenant_id, (Instant::now(), ancestors.clone()));
        ancestors
    }
}

/// A fixed hierarchy, for tests and for single-tenant deployments.
#[derive(Debug, Default)]
pub struct StaticTenantChain {
    parents: DashMap<Uuid, Uuid>,
}

impl StaticTenantChain {
    /// An empty hierarchy: every tenant is a root.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `parent` as the parent of `child`.
    #[must_use]
    pub fn with_parent(self, child: Uuid, parent: Uuid) -> Self {
        self.parents.insert(child, parent);
        self
    }
}

#[async_trait]
impl TenantChain for StaticTenantChain {
    async fn ancestors(&self, _ctx: &SecurityContext, tenant_id: Uuid) -> Vec<Uuid> {
        let mut chain = Vec::new();
        let mut current = tenant_id;
        // Bounded so a mis-seeded cycle cannot hang a request.
        for _ in 0..64 {
            let Some(parent) = self.parents.get(&current).map(|e| *e.value()) else {
                break;
            };
            if chain.contains(&parent) || parent == tenant_id {
                break;
            }
            chain.push(parent);
            current = parent;
        }
        chain
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn static_chain_walks_to_the_root() {
        let root = Uuid::new_v4();
        let mid = Uuid::new_v4();
        let leaf = Uuid::new_v4();
        let hierarchy = StaticTenantChain::new()
            .with_parent(leaf, mid)
            .with_parent(mid, root);
        let ctx = SecurityContext::anonymous();

        assert_eq!(hierarchy.ancestors(&ctx, leaf).await, vec![mid, root]);
        assert_eq!(hierarchy.chain(&ctx, leaf).await, vec![leaf, mid, root]);
        assert!(hierarchy.ancestors(&ctx, root).await.is_empty());
    }

    #[tokio::test]
    async fn static_chain_tolerates_a_cycle() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let hierarchy = StaticTenantChain::new().with_parent(a, b).with_parent(b, a);
        let ctx = SecurityContext::anonymous();
        assert_eq!(hierarchy.ancestors(&ctx, a).await, vec![b]);
    }
}
