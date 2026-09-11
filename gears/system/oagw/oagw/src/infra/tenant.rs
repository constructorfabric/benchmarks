//! Tenant hierarchy directory backed by the tenant-resolver gear.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dashmap::DashMap;
use tenant_resolver_sdk::{GetAncestorsOptions, TenantId, TenantResolverClient};
use toolkit_security::SecurityContext;
use tracing::warn;
use uuid::Uuid;

use crate::domain::tenant::TenantDirectory;

#[derive(Debug, Clone)]
struct CachedChain {
    chain: Vec<Uuid>,
    fetched_at: Instant,
}

/// Caches the descendant → root chain for a short TTL.
///
/// Every proxy request needs the chain, and the hierarchy changes rarely; a
/// short TTL keeps the hot path off the resolver without holding a stale view
/// for long.
pub struct TenantResolverDirectory {
    client: Arc<dyn TenantResolverClient>,
    cache: DashMap<Uuid, CachedChain>,
    ttl: Duration,
}

impl std::fmt::Debug for TenantResolverDirectory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TenantResolverDirectory")
            .field("ttl", &self.ttl)
            .field("cached", &self.cache.len())
            .finish_non_exhaustive()
    }
}

impl TenantResolverDirectory {
    #[must_use]
    pub fn new(client: Arc<dyn TenantResolverClient>, ttl_secs: u64) -> Self {
        Self {
            client,
            cache: DashMap::new(),
            ttl: Duration::from_secs(ttl_secs.max(1)),
        }
    }

    /// Drop every cached chain — used when the hierarchy is known to have
    /// changed.
    pub fn invalidate(&self) {
        self.cache.clear();
    }
}

#[async_trait]
impl TenantDirectory for TenantResolverDirectory {
    async fn ancestor_chain(&self, ctx: &SecurityContext, tenant_id: Uuid) -> Vec<Uuid> {
        if let Some(entry) = self.cache.get(&tenant_id)
            && entry.fetched_at.elapsed() < self.ttl
        {
            return entry.chain.clone();
        }

        let response = self
            .client
            .get_ancestors(ctx, TenantId(tenant_id), &GetAncestorsOptions::default())
            .await;

        let chain = match response {
            Ok(response) => {
                let mut chain = Vec::with_capacity(response.ancestors.len() + 1);
                chain.push(tenant_id);
                chain.extend(response.ancestors.iter().map(|ancestor| ancestor.id.0));
                chain
            }
            Err(err) => {
                // Degrade to the tenant's own scope rather than failing every
                // proxy request: a resolver outage must not take out upstreams
                // the tenant owns outright.
                warn!(
                    target: "oagw.tenant",
                    tenant_id = %tenant_id,
                    error = %err,
                    "tenant hierarchy lookup failed; falling back to a single-tenant chain"
                );
                vec![tenant_id]
            }
        };

        self.cache.insert(
            tenant_id,
            CachedChain {
                chain: chain.clone(),
                fetched_at: Instant::now(),
            },
        );
        chain
    }
}
