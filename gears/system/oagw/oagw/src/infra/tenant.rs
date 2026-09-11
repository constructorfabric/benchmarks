//! Tenant hierarchy lookup backed by the tenant-resolver gear, with a short
//! TTL cache so the proxy hot path does not re-walk the tree per request.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dashmap::DashMap;
use tenant_resolver_sdk::{GetAncestorsOptions, TenantId, TenantResolverClient};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::ports::TenantDirectory;

/// A cached ancestor chain.
struct CachedChain {
    chain: Vec<Uuid>,
    fetched_at: Instant,
}

/// [`TenantDirectory`] over the tenant-resolver SDK.
pub struct TenantResolverDirectory {
    client: Arc<dyn TenantResolverClient>,
    cache: DashMap<Uuid, CachedChain>,
    ttl: Duration,
}

impl TenantResolverDirectory {
    /// Bind the directory to the resolver client.
    #[must_use]
    pub fn new(client: Arc<dyn TenantResolverClient>, ttl: Duration) -> Self {
        Self {
            client,
            cache: DashMap::new(),
            ttl,
        }
    }
}

#[async_trait]
impl TenantDirectory for TenantResolverDirectory {
    async fn chain(&self, ctx: &SecurityContext, tenant_id: Uuid) -> Vec<Uuid> {
        if let Some(entry) = self.cache.get(&tenant_id)
            && entry.fetched_at.elapsed() < self.ttl
        {
            return entry.chain.clone();
        }

        let mut chain = vec![tenant_id];
        match self
            .client
            .get_ancestors(ctx, TenantId(tenant_id), &GetAncestorsOptions::default())
            .await
        {
            Ok(response) => {
                chain.extend(response.ancestors.iter().map(|a| a.id.0));
            }
            Err(err) => {
                // Degrade to "no inheritance" rather than failing the request:
                // the tenant's own configuration is still authoritative.
                tracing::warn!(
                    target: "oagw.tenant",
                    tenant_id = %tenant_id,
                    error = %err,
                    "tenant ancestor lookup failed; proceeding without hierarchy inheritance"
                );
            }
        }
        chain.dedup();
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

/// A directory that reports every tenant as a root. Used when the
/// tenant-resolver client is unavailable, and by tests that do not care about
/// inheritance.
#[derive(Debug, Default)]
pub struct FlatTenantDirectory;

#[async_trait]
impl TenantDirectory for FlatTenantDirectory {
    async fn chain(&self, _ctx: &SecurityContext, tenant_id: Uuid) -> Vec<Uuid> {
        vec![tenant_id]
    }
}

#[cfg(test)]
mod tests {
    use super::{FlatTenantDirectory, TenantResolverDirectory};
    use crate::domain::ports::TenantDirectory;
    use async_trait::async_trait;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tenant_resolver_sdk::{
        GetAncestorsOptions, GetAncestorsResponse, GetDescendantsOptions, GetDescendantsResponse,
        GetTenantsOptions, IsAncestorOptions, TenantId, TenantInfo, TenantRef,
        TenantResolverClient, TenantResolverError, TenantStatus,
    };
    use toolkit_security::SecurityContext;
    use uuid::Uuid;

    struct FakeResolver {
        /// `child -> parent` edges.
        parents: Vec<(Uuid, Uuid)>,
        calls: AtomicUsize,
    }

    fn tenant_ref(id: Uuid) -> TenantRef {
        TenantRef {
            id: TenantId(id),
            status: TenantStatus::Active,
            tenant_type: None,
            parent_id: None,
            self_managed: false,
        }
    }

    #[async_trait]
    impl TenantResolverClient for FakeResolver {
        async fn get_tenant(
            &self,
            _ctx: &SecurityContext,
            id: TenantId,
        ) -> Result<TenantInfo, TenantResolverError> {
            Ok(TenantInfo {
                id,
                name: "t".to_owned(),
                status: TenantStatus::Active,
                tenant_type: None,
                parent_id: None,
                self_managed: false,
            })
        }

        async fn get_root_tenant(
            &self,
            ctx: &SecurityContext,
        ) -> Result<TenantInfo, TenantResolverError> {
            self.get_tenant(ctx, TenantId(Uuid::nil())).await
        }

        async fn get_tenants(
            &self,
            _ctx: &SecurityContext,
            _ids: &[TenantId],
            _options: &GetTenantsOptions,
        ) -> Result<Vec<TenantInfo>, TenantResolverError> {
            Ok(Vec::new())
        }

        async fn get_ancestors(
            &self,
            _ctx: &SecurityContext,
            id: TenantId,
            _options: &GetAncestorsOptions,
        ) -> Result<GetAncestorsResponse, TenantResolverError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut ancestors = Vec::new();
            let mut current = id.0;
            while let Some((_, parent)) = self.parents.iter().find(|(c, _)| *c == current) {
                ancestors.push(tenant_ref(*parent));
                current = *parent;
            }
            Ok(GetAncestorsResponse {
                tenant: tenant_ref(id.0),
                ancestors,
            })
        }

        async fn get_descendants(
            &self,
            _ctx: &SecurityContext,
            id: TenantId,
            _options: &GetDescendantsOptions,
        ) -> Result<GetDescendantsResponse, TenantResolverError> {
            Ok(GetDescendantsResponse {
                tenant: tenant_ref(id.0),
                descendants: Vec::new(),
            })
        }

        async fn is_ancestor(
            &self,
            _ctx: &SecurityContext,
            _ancestor_id: TenantId,
            _descendant_id: TenantId,
            _options: &IsAncestorOptions,
        ) -> Result<bool, TenantResolverError> {
            Ok(false)
        }
    }

    struct BrokenResolver;

    #[async_trait]
    impl TenantResolverClient for BrokenResolver {
        async fn get_tenant(
            &self,
            _ctx: &SecurityContext,
            _id: TenantId,
        ) -> Result<TenantInfo, TenantResolverError> {
            Err(TenantResolverError::Internal("down".to_owned()))
        }
        async fn get_root_tenant(
            &self,
            _ctx: &SecurityContext,
        ) -> Result<TenantInfo, TenantResolverError> {
            Err(TenantResolverError::Internal("down".to_owned()))
        }
        async fn get_tenants(
            &self,
            _ctx: &SecurityContext,
            _ids: &[TenantId],
            _options: &GetTenantsOptions,
        ) -> Result<Vec<TenantInfo>, TenantResolverError> {
            Err(TenantResolverError::Internal("down".to_owned()))
        }
        async fn get_ancestors(
            &self,
            _ctx: &SecurityContext,
            _id: TenantId,
            _options: &GetAncestorsOptions,
        ) -> Result<GetAncestorsResponse, TenantResolverError> {
            Err(TenantResolverError::Internal("down".to_owned()))
        }
        async fn get_descendants(
            &self,
            _ctx: &SecurityContext,
            _id: TenantId,
            _options: &GetDescendantsOptions,
        ) -> Result<GetDescendantsResponse, TenantResolverError> {
            Err(TenantResolverError::Internal("down".to_owned()))
        }
        async fn is_ancestor(
            &self,
            _ctx: &SecurityContext,
            _ancestor_id: TenantId,
            _descendant_id: TenantId,
            _options: &IsAncestorOptions,
        ) -> Result<bool, TenantResolverError> {
            Err(TenantResolverError::Internal("down".to_owned()))
        }
    }

    #[tokio::test]
    async fn chain_runs_descendant_to_root() {
        let root = Uuid::new_v4();
        let mid = Uuid::new_v4();
        let leaf = Uuid::new_v4();
        let resolver = Arc::new(FakeResolver {
            parents: vec![(leaf, mid), (mid, root)],
            calls: AtomicUsize::new(0),
        });
        let dir = TenantResolverDirectory::new(resolver, Duration::from_secs(30));
        let chain = dir.chain(&SecurityContext::anonymous(), leaf).await;
        assert_eq!(chain, vec![leaf, mid, root]);
    }

    #[tokio::test]
    async fn the_chain_is_cached_within_its_ttl() {
        let root = Uuid::new_v4();
        let leaf = Uuid::new_v4();
        let resolver = Arc::new(FakeResolver {
            parents: vec![(leaf, root)],
            calls: AtomicUsize::new(0),
        });
        let client: Arc<dyn TenantResolverClient> = Arc::clone(&resolver) as _;
        let dir = TenantResolverDirectory::new(client, Duration::from_secs(30));
        let ctx = SecurityContext::anonymous();
        dir.chain(&ctx, leaf).await;
        dir.chain(&ctx, leaf).await;
        assert_eq!(
            resolver.calls.load(Ordering::SeqCst),
            1,
            "the second lookup must be served from cache"
        );
    }

    #[tokio::test]
    async fn a_resolver_failure_degrades_to_no_inheritance() {
        let dir = TenantResolverDirectory::new(Arc::new(BrokenResolver), Duration::from_secs(30));
        let tenant = Uuid::new_v4();
        assert_eq!(
            dir.chain(&SecurityContext::anonymous(), tenant).await,
            vec![tenant],
            "the tenant's own configuration must still resolve"
        );
    }

    #[tokio::test]
    async fn the_flat_directory_reports_every_tenant_as_a_root() {
        let tenant = Uuid::new_v4();
        assert_eq!(
            FlatTenantDirectory
                .chain(&SecurityContext::anonymous(), tenant)
                .await,
            vec![tenant]
        );
    }
}
