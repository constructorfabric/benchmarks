//! The gear: registration, configuration and route wiring.

use std::sync::Arc;

use toolkit::{Gear, GearCtx};
use toolkit_security::SecurityContext;

use crate::config::OagwConfig;
use crate::ratelimit::{RateLimiter, SystemClock};
use crate::security::{NoopCredentialResolver, StoreCredentialResolver};
use crate::store::OagwStore;

/// The gear, registered with the platform's inventory.
#[toolkit::gear(
    name = "oagw",
    deps = [tenant_resolver],
    capabilities = [rest]
)]
pub struct OagwGear {
    runtime: std::sync::OnceLock<Arc<crate::proxy::ProxyService>>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            runtime: std::sync::OnceLock::new(),
        }
    }
}

#[async_trait::async_trait]
impl Gear for OagwGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let config: OagwConfig = ctx.config_or_default()?;
        let store = Arc::new(OagwStore::new());

        let credentials: Arc<dyn crate::security::CredentialResolver> = if let Ok(client) = ctx
            .client_hub()
            .get::<dyn credstore_sdk::CredStoreClientV1>()
        {
            Arc::new(StoreCredentialResolver::new(client))
        } else {
            tracing::warn!(
                target: "oagw",
                "no credential store is wired; every auth plugin referencing one will fail"
            );
            Arc::new(NoopCredentialResolver)
        };

        let tenants = ctx
            .client_hub()
            .get::<dyn tenant_resolver_sdk::TenantResolverClient>()
            .ok();

        let cache = Arc::new(crate::plugins::token_cache::TokenCache::from_config(
            &config.token_cache,
        ));
        let limiter = Arc::new(RateLimiter::new(Arc::new(SystemClock)));

        let client = crate::proxy::build_client()?;
        let runtime = Arc::new(crate::proxy::ProxyService::new(
            store,
            credentials,
            cache,
            limiter,
            config,
            tenants,
            client,
        ));
        self.runtime
            .set(runtime)
            .map_err(|_| anyhow::anyhow!("oagw: init called twice"))?;
        tracing::info!(target: "oagw", "oagw gear initialised");
        Ok(())
    }
}

impl toolkit::RestApiCapability for OagwGear {
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn toolkit::api::OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        let svc = self
            .runtime
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("oagw: register_rest invoked before init"))?;
        tracing::info!(target: "oagw", "registering oagw REST routes");
        Ok(crate::api::register_routes(router, openapi, svc))
    }
}

/// Resolves the caller's tenant chain for a handler that has not been routed through the
/// data plane.
///
/// # Errors
///
/// Returns the resolver's error when one is wired and it fails.
pub async fn chain_for(
    runtime: &crate::proxy::ProxyService,
    ctx: &SecurityContext,
) -> anyhow::Result<crate::store::TenantChain> {
    runtime.chain_for(ctx).await
}
