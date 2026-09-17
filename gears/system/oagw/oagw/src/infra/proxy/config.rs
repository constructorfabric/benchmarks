//! Read-side source of the upstream and route configuration the proxy needs.
//!
//! The management plane is strictly per-tenant; the data plane is not: at
//! proxy time an ancestor's upstream and its routes are inherited
//! (`docs/DESIGN.md` — "Proxy (data plane) … Inherited via tenant chain
//! walk"). [`ConfigSource`] therefore walks the tenant chain and returns a
//! caller-first list, which is the order [`crate::infra::proxy::route::resolve_alias`]
//! expects.
//!
//! The tenant resolver is optional: when the platform did not register one,
//! the chain is the caller's own tenant only.
use async_trait::async_trait;
use tenant_resolver_sdk::TenantResolverClient;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::model::{Route, Upstream};
use crate::domain::repo::ConfigStore;

/// Ancestor lookup for alias resolution.
#[async_trait]
pub trait TenantChain: Send + Sync {
    /// Ancestors of `tenant_id`, closest parent first.
    ///
    /// A resolver that cannot answer returns an empty list rather than an
    /// error: the proxy still serves the caller's own upstreams.
    async fn ancestors(&self, tenant_id: Uuid) -> Vec<Uuid>;
}

/// Ancestor lookup over the `tenant-resolver` gear's client.
pub struct ResolverChain {
    resolver: Option<std::sync::Arc<dyn TenantResolverClient>>,
}

impl ResolverChain {
    /// Wrap a resolver; `None` yields an empty chain.
    #[must_use]
    pub fn new(resolver: Option<std::sync::Arc<dyn TenantResolverClient>>) -> Self {
        Self { resolver }
    }
}

#[async_trait]
impl TenantChain for ResolverChain {
    async fn ancestors(&self, tenant_id: Uuid) -> Vec<Uuid> {
        let Some(resolver) = self.resolver.as_ref() else {
            return Vec::new();
        };
        let Ok(context) = SecurityContext::builder()
            .subject_tenant_id(tenant_id)
            .subject_id(tenant_id)
            .build()
        else {
            return Vec::new();
        };
        let options = tenant_resolver_sdk::models::GetAncestorsOptions::default();
        match resolver
            .get_ancestors(
                &context,
                tenant_resolver_sdk::models::TenantId(tenant_id),
                &options,
            )
            .await
        {
            Ok(response) => response
                .ancestors
                .iter()
                .map(|tenant| tenant.id.0)
                .collect(),
            Err(error) => {
                tracing::warn!(tenant = %tenant_id, error = %error, "oagw: cannot resolve the tenant chain");
                Vec::new()
            }
        }
    }
}

/// Read-side configuration source of the data plane.
#[derive(Clone)]
pub struct ConfigSource {
    store: std::sync::Arc<dyn ConfigStore>,
    chain: std::sync::Arc<dyn TenantChain>,
}

impl ConfigSource {
    /// Build a source over a store and an optional tenant resolver.
    #[must_use]
    pub fn new(
        store: std::sync::Arc<dyn ConfigStore>,
        chain: std::sync::Arc<dyn TenantChain>,
    ) -> Self {
        Self { store, chain }
    }

    /// The upstreams the caller's tenant may proxy through, closest first.
    #[must_use]
    pub async fn chain(&self, tenant_id: &Uuid) -> Vec<Upstream> {
        let mut chain = self.store.list_upstreams(*tenant_id).unwrap_or_default();
        for ancestor in self.chain.ancestors(*tenant_id).await {
            chain.extend(self.store.list_upstreams(ancestor).unwrap_or_default());
        }
        chain
    }

    /// The routes visible to the caller's tenant.
    #[must_use]
    pub async fn routes(&self, tenant_id: &Uuid) -> Vec<Route> {
        self.store.list_routes(*tenant_id).unwrap_or_default()
    }

    /// The routes of every tenant in the chain, closest first.
    #[must_use]
    pub async fn chain_routes(&self, tenant_id: &Uuid) -> Vec<Route> {
        let mut routes = self.store.list_routes(*tenant_id).unwrap_or_default();
        for ancestor in self.chain.ancestors(*tenant_id).await {
            routes.extend(self.store.list_routes(ancestor).unwrap_or_default());
        }
        routes
    }
}
