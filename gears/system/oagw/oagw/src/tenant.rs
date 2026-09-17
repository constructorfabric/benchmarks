//! Tenant hierarchy resolution for the OAGW control/data plane.
//!
//! Alias resolution and effective-configuration merge walk the tenant chain
//! from the calling tenant to the root (`descendant → root`). When the
//! `tenant-resolver` gear is reachable through the client hub, the chain is
//! resolved from it (cached); otherwise a locally seeded parent map is used
//! so the gear remains fully functional in isolation and under tests.

use std::sync::Arc;

use dashmap::DashMap;
use tenant_resolver_sdk::{GetAncestorsOptions, TenantId, TenantResolverClient};
use toolkit_security::SecurityContext;
use uuid::Uuid;

/// Resolves tenant ancestor chains.
#[derive(Default)]
pub struct TenantChain {
    resolver: Option<Arc<dyn TenantResolverClient>>,
    /// Local parent edges (child → parent), usable as a fallback when no
    /// resolver is registered (and for tests to seed hierarchies).
    local_parent: DashMap<Uuid, Uuid>,
    cache: DashMap<Uuid, Vec<Uuid>>,
}

impl TenantChain {
    /// Attach the platform tenant resolver (from the client hub).
    pub fn with_resolver(resolver: Arc<dyn TenantResolverClient>) -> Self {
        Self {
            resolver: Some(resolver),
            local_parent: DashMap::new(),
            cache: DashMap::new(),
        }
    }

    /// Seed a local parent edge (child → parent). Also invalidates any
    /// cached chains containing `child` or `parent`.
    pub fn set_local_parent(&self, child: Uuid, parent: Uuid) {
        self.cache.retain(|_, chain| {
            !chain.contains(&child) && !chain.contains(&parent)
        });
        self.local_parent.insert(child, parent);
    }

    /// Ancestor chain from `tenant` up to the root (inclusive), ordered
    /// nearest-first: `[tenant, parent, ..., root]`.
    pub async fn ancestors(
        &self,
        tenant: Uuid,
        ctx: &SecurityContext,
    ) -> Vec<Uuid> {
        if let Some(cached) = self.cache.get(&tenant) {
            return cached.clone();
        }

        let chain = self.resolve(tenant, ctx).await;
        self.cache.insert(tenant, chain.clone());
        chain
    }

    async fn resolve(&self, tenant: Uuid, ctx: &SecurityContext) -> Vec<Uuid> {
        // Local parent map provides a deterministic fast path (tests,
        // single-tenant contexts, and resolver-less deployments).
        if self.local_parent.is_empty() && self.resolver.is_none() {
            return vec![tenant];
        }

        if let Some(resolver) = &self.resolver {
            let options = GetAncestorsOptions::default();
            if let Ok(resp) = resolver.get_ancestors(ctx, TenantId(tenant), &options).await {
                let mut chain = Vec::with_capacity(resp.ancestors.len() + 1);
                chain.push(tenant);
                chain.extend(resp.ancestors.iter().map(|t| (t.id).0));
                if !chain.is_empty() {
                    return chain;
                }
            }
        }

        // Fallback: walk the local parent map.
        let mut chain = Vec::new();
        let mut current = tenant;
        let mut guard = 0;
        while guard < 64 {
            chain.push(current);
            match self.local_parent.get(&current) {
                Some(parent) => {
                    let parent = *parent;
                    if parent == current {
                        break;
                    }
                    current = parent;
                }
                None => break,
            }
            guard += 1;
        }
        chain
    }
}
