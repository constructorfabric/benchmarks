//! Infra adapter: the tenant hierarchy of the management state, backed by
//! tenant-resolver.
//!
//! [`crate::domain::sharing::TenantHierarchy`] is the domain port; this module
//! is the only implementation that reaches outside the process. It resolves the
//! caller's ancestor chain through the `TenantResolverClient` the gear found in
//! the client hub, with a small TTL cache so a management write does not pay a
//! resolver round trip on every request.
//!
//! The adapter is deliberately thin: the resolver owns the topology and its
//! access-control decision, and this side only projects the chain the sharing
//! gate walks (`cpt-cf-oagw-algo-sharing-mode-validate`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tenant_resolver_sdk::error::TenantResolverError;
use tenant_resolver_sdk::models::{GetAncestorsOptions, TenantId};
use tenant_resolver_sdk::TenantResolverClient;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::ManagementError;
use crate::domain::sharing::TenantHierarchy;

/// Maximum cached chains before a full eviction.
const CACHE_CAP: usize = 4096;

/// Entry: `(chain, inserted_at)`.
type CacheEntry = (Vec<Uuid>, Instant);

/// The ancestor chain of a tenant, resolved through tenant-resolver.
///
/// Chains are keyed by tenant id alone: a chain is a pure function of the
/// tenant topology and the barrier mode, so it carries no caller-specific data
/// and every authorization stays with the resolver and the sharing gate
/// downstream.
pub struct ResolverHierarchy {
    client: Arc<dyn TenantResolverClient>,
    cache: Mutex<HashMap<Uuid, CacheEntry>>,
    ttl: Duration,
}

impl ResolverHierarchy {
    /// Resolve chains through `client`, caching them for `ttl_secs`.
    #[must_use]
    pub fn new(client: Arc<dyn TenantResolverClient>, ttl_secs: u64) -> Self {
        Self {
            client,
            cache: Mutex::new(HashMap::new()),
            ttl: Duration::from_secs(ttl_secs),
        }
    }

    /// The cached chain for `tenant_id`, when one is still fresh.
    fn cached(&self, tenant_id: Uuid) -> Option<Vec<Uuid>> {
        let guard = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .get(&tenant_id)
            .filter(|(_, inserted)| inserted.elapsed() < self.ttl)
            .cloned()
            .map(|(chain, _)| chain)
    }

    /// Store a freshly resolved chain, evicting expired entries at the cap.
    fn store(&self, tenant_id: Uuid, chain: Vec<Uuid>) {
        let mut guard = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.len() >= CACHE_CAP {
            let ttl = self.ttl;
            guard.retain(|_, (_, inserted)| inserted.elapsed() < ttl);
        }
        guard.insert(tenant_id, (chain, Instant::now()));
    }
}

#[async_trait]
impl TenantHierarchy for ResolverHierarchy {
    async fn ancestors_of(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
    ) -> Result<Vec<Uuid>, ManagementError> {
        if let Some(chain) = self.cached(tenant_id) {
            return Ok(chain);
        }

        // `Respect` is the semantically correct traversal for configuration
        // inheritance: a self-managed sub-tree owns its configuration, so a
        // barrier ends the chain and the descendant inherits nothing across it.
        // (credstore asks the opposite question of the same topology and uses
        // `Ignore` for secret inheritance.)
        let options = GetAncestorsOptions {
            barrier_mode: tenant_resolver_sdk::BarrierMode::Respect,
        };
        let response = self
            .client
            .get_ancestors(ctx, TenantId(tenant_id), &options)
            .await
            .map_err(fail_closed)?;

        // The resolver returns the requested tenant first; the gate walks
        // ancestors only, so self is dropped here.
        let chain: Vec<Uuid> = response.ancestors.iter().map(|a| a.id.0).collect();
        self.store(tenant_id, chain.clone());
        Ok(chain)
    }
}

/// Map a resolver failure to the `503` that fails a management write closed.
fn fail_closed(error: TenantResolverError) -> ManagementError {
    // Wire-visible detail stays curated; the raw dependency error goes to the
    // log and the cause chain only.
    tracing::warn!(err = %error, "tenant_resolver get_ancestors failed");
    ManagementError::from(crate::domain::DomainError::LinkUnavailable {
        detail: "tenant hierarchy source unavailable".to_string(),
        retry_after_seconds: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use tenant_resolver_sdk::models::{
        GetAncestorsResponse, GetDescendantsOptions, GetDescendantsResponse, GetTenantsOptions,
        IsAncestorOptions, TenantInfo, TenantRef, TenantStatus,
    };
    use uuid::Uuid;

    /// A resolver stub that returns a fixed chain per tenant, or fails.
    struct StubResolver {
        chains: HashMap<Uuid, Vec<Uuid>>,
        calls: std::sync::atomic::AtomicUsize,
        fail: bool,
    }

    impl StubResolver {
        fn new(chains: HashMap<Uuid, Vec<Uuid>>, fail: bool) -> Self {
            Self {
                chains,
                calls: std::sync::atomic::AtomicUsize::new(0),
                fail,
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn reference(id: Uuid) -> TenantRef {
            TenantRef {
                id: TenantId(id),
                status: TenantStatus::Active,
                tenant_type: None,
                parent_id: None,
                self_managed: false,
            }
        }
    }

    #[async_trait]
    impl TenantResolverClient for StubResolver {
        async fn get_tenant(
            &self,
            _ctx: &SecurityContext,
            _id: TenantId,
        ) -> Result<TenantInfo, TenantResolverError> {
            Err(TenantResolverError::NoPluginAvailable)
        }

        async fn get_root_tenant(
            &self,
            _ctx: &SecurityContext,
        ) -> Result<TenantInfo, TenantResolverError> {
            Err(TenantResolverError::NoPluginAvailable)
        }

        async fn get_tenants(
            &self,
            _ctx: &SecurityContext,
            _ids: &[TenantId],
            _options: &GetTenantsOptions,
        ) -> Result<Vec<TenantInfo>, TenantResolverError> {
            Err(TenantResolverError::NoPluginAvailable)
        }

        async fn get_ancestors(
            &self,
            _ctx: &SecurityContext,
            id: TenantId,
            _options: &GetAncestorsOptions,
        ) -> Result<GetAncestorsResponse, TenantResolverError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.fail {
                return Err(TenantResolverError::ServiceUnavailable(
                    "resolver down".to_string(),
                ));
            }
            let ancestors = self
                .chains
                .get(&id.0)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(Self::reference)
                .collect();
            Ok(GetAncestorsResponse {
                tenant: Self::reference(id.0),
                ancestors,
            })
        }

        async fn get_descendants(
            &self,
            _ctx: &SecurityContext,
            _id: TenantId,
            _options: &GetDescendantsOptions,
        ) -> Result<GetDescendantsResponse, TenantResolverError> {
            Err(TenantResolverError::NoPluginAvailable)
        }

        async fn is_ancestor(
            &self,
            _ctx: &SecurityContext,
            _ancestor_id: TenantId,
            _descendant_id: TenantId,
            _options: &IsAncestorOptions,
        ) -> Result<bool, TenantResolverError> {
            Err(TenantResolverError::NoPluginAvailable)
        }
    }

    #[tokio::test]
    async fn a_resolver_chain_reaches_the_port() {
        let ancestor = uuid::uuid!("11111111-1111-1111-1111-111111111111");
        let tenant = uuid::uuid!("22222222-2222-2222-2222-222222222222");
        let resolver = Arc::new(StubResolver::new(HashMap::from([(tenant, vec![ancestor])]), false));
        let hierarchy = ResolverHierarchy::new(Arc::clone(&resolver) as Arc<dyn TenantResolverClient>, 60);

        let chain = hierarchy
            .ancestors_of(&SecurityContext::anonymous(), tenant)
            .await
            .expect("chain");
        assert_eq!(chain, vec![ancestor], "self is excluded from the chain");
    }

    #[tokio::test]
    async fn the_chain_is_cached_for_the_ttl() {
        let ancestor = uuid::uuid!("11111111-1111-1111-1111-111111111111");
        let tenant = uuid::uuid!("22222222-2222-2222-2222-222222222222");
        let resolver = Arc::new(StubResolver::new(HashMap::from([(tenant, vec![ancestor])]), false));
        let hierarchy = ResolverHierarchy::new(Arc::clone(&resolver) as Arc<dyn TenantResolverClient>, 60);

        for _ in 0..3 {
            let chain = hierarchy
                .ancestors_of(&SecurityContext::anonymous(), tenant)
                .await
                .expect("chain");
            assert_eq!(chain, vec![ancestor]);
        }
        assert_eq!(resolver.calls(), 1, "one resolver call, cache hits after");
    }

    #[tokio::test]
    async fn a_tenant_without_ancestors_resolves_to_an_empty_chain() {
        let tenant = uuid::uuid!("44444444-4444-4444-4444-444444444444");
        let resolver = Arc::new(StubResolver::new(HashMap::new(), false));
        let hierarchy = ResolverHierarchy::new(Arc::clone(&resolver) as Arc<dyn TenantResolverClient>, 60);

        let chain = hierarchy
            .ancestors_of(&SecurityContext::anonymous(), tenant)
            .await
            .expect("chain");
        assert!(chain.is_empty(), "a root tenant has no ancestors");
    }

    #[tokio::test]
    async fn an_unreachable_resolver_fails_closed() {
        let tenant = uuid::uuid!("33333333-3333-3333-3333-333333333333");
        let resolver = Arc::new(StubResolver::new(HashMap::new(), true));
        let hierarchy = ResolverHierarchy::new(Arc::clone(&resolver) as Arc<dyn TenantResolverClient>, 60);

        let error = hierarchy
            .ancestors_of(&SecurityContext::anonymous(), tenant)
            .await
            .expect_err("mapped 503");
        assert_eq!(error.status(), 503, "{error}");
        assert_eq!(error.gts_id(), "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1");
    }
}
