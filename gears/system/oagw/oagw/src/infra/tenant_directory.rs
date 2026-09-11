//! Tenant hierarchy lookups backed by the `tenant-resolver` gear.
//!
//! Results are cached briefly: alias resolution walks the chain on every proxy
//! request and the hierarchy changes rarely.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dashmap::DashMap;
use tenant_resolver_sdk::{GetAncestorsOptions, TenantId, TenantResolverClient};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::repo::TenantDirectory;

/// `tenant-resolver`-backed directory with a short-TTL chain cache.
pub struct TenantResolverDirectory {
    client: Arc<dyn TenantResolverClient>,
    cache: DashMap<Uuid, (Instant, Vec<Uuid>)>,
    ttl: Duration,
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
}

#[async_trait]
impl TenantDirectory for TenantResolverDirectory {
    async fn chain(&self, ctx: &SecurityContext, tenant_id: Uuid) -> Vec<Uuid> {
        if let Some(entry) = self.cache.get(&tenant_id)
            && entry.0.elapsed() < self.ttl
        {
            return entry.1.clone();
        }

        let mut chain = vec![tenant_id];
        match self
            .client
            .get_ancestors(ctx, TenantId(tenant_id), &GetAncestorsOptions::default())
            .await
        {
            Ok(resp) => {
                for ancestor in resp.ancestors {
                    if !chain.contains(&ancestor.id.0) {
                        chain.push(ancestor.id.0);
                    }
                }
            }
            Err(err) => {
                // Degrade to the tenant's own scope: an ancestor lookup outage
                // must not take tenant-local upstreams offline.
                tracing::warn!(
                    target: "oagw.tenancy",
                    tenant_id = %tenant_id,
                    error = %err,
                    "tenant ancestor resolution failed; falling back to a single-tenant chain"
                );
            }
        }
        self.cache.insert(tenant_id, (Instant::now(), chain.clone()));
        chain
    }
}

/// Directory used when no `tenant-resolver` client is available: every tenant
/// is its own root.
pub struct FlatTenantDirectory;

#[async_trait]
impl TenantDirectory for FlatTenantDirectory {
    async fn chain(&self, _ctx: &SecurityContext, tenant_id: Uuid) -> Vec<Uuid> {
        vec![tenant_id]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_flat_directory_returns_only_the_caller() {
        let dir = FlatTenantDirectory;
        let id = Uuid::new_v4();
        let ctx = SecurityContext::anonymous();
        assert_eq!(dir.chain(&ctx, id).await, vec![id]);
    }
}
