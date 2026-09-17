//! Gear declaration for the OAGW gear.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use credstore_sdk::CredStoreClientV1;
use tenant_resolver_sdk::TenantResolverClient;
use toolkit::api::OpenApiRegistry;
use toolkit::{Gear, GearCtx, RestApiCapability};
use toolkit_http::{HttpClientBuilder, HttpClientConfig, TransportSecurity};
use tracing::{info, warn};

use crate::config::OagwConfig;
use crate::infra::plugins::TokenCacheConfig;
use crate::infra::storage::{Services, SingleTenantChain, TenantResolverChain};
use crate::infra::storage::TenantChainProvider;

/// OAGW gear.
///
/// Centralized outbound proxy layer:
/// - Control plane: upstream / route / plugin CRUD at `/oagw/v1`.
/// - Data plane: alias-based proxying at `/oagw/v1/proxy/{alias}/{path}`.
#[toolkit::gear(
    name = "oagw",
    capabilities = [rest]
)]
pub struct OagwGear {
    services: OnceLock<Arc<Services>>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            services: OnceLock::new(),
        }
    }
}

#[async_trait]
impl Gear for OagwGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: OagwConfig = ctx.config_or_default()?;
        info!(
            proxy_timeout_secs = cfg.proxy_timeout_secs,
            allow_http_upstream = cfg.allow_http_upstream,
            "Initializing oagw gear"
        );

        // Credential store for auth plugin secret material.
        let credstore: Arc<dyn CredStoreClientV1> =
            ctx.client_hub().get::<dyn CredStoreClientV1>().map_err(|e| {
                anyhow::anyhow!("oagw: credstore client unavailable: {e}")
            })?;

        // Tenant resolver for the hierarchical chain (optional: single-tenant
        // deployments run without it).
        let tenant_chain: Arc<dyn TenantChainProvider> =
            match ctx.client_hub().get::<dyn TenantResolverClient>() {
                Ok(resolver) => Arc::new(TenantResolverChain::new(resolver)),
                Err(_) => {
                    warn!("oagw: tenant-resolver client unavailable; using single-tenant chain");
                    Arc::new(SingleTenantChain(uuid::Uuid::nil()))
                }
            };

        // Outgoing HTTP client honoring the gear's proxy timeout and the
        // allow-http-upstream escape hatch (SSRF policy is enforced by the
        // data plane when configured).
        let mut http_cfg = HttpClientConfig::proxy();
        http_cfg.request_timeout = Duration::from_secs(cfg.proxy_timeout_secs.max(1));
        if cfg.allow_http_upstream && !cfg.ssrf_policy.enabled {
            http_cfg.transport = TransportSecurity::AllowInsecureHttp;
        } else {
            http_cfg.transport = TransportSecurity::TlsOnly;
        }
        let outgoing = HttpClientBuilder::with_config(http_cfg)
            .build()
            .map_err(|e| anyhow::anyhow!("oagw: failed to build outgoing HTTP client: {e}"))?;

        let services = Arc::new(Services {
            config: cfg.clone(),
            tenant_chain,
            credstore: credstore.clone(),
            outgoing_client: outgoing,
            limiter: crate::infra::rate_limiter::RateLimiter::new(),
            round_robin: std::sync::atomic::AtomicUsize::new(0),
            auth_plugins: crate::infra::plugins::AuthPluginRegistry::with_builtins(
                credstore,
                HttpClientConfig::proxy(),
                TokenCacheConfig {
                    ttl: cfg.token_cache_ttl(),
                    capacity: cfg.token_cache_capacity,
                },
            ),
            upstreams: dashmap::DashMap::new(),
            upstream_by_alias: dashmap::DashMap::new(),
            routes: dashmap::DashMap::new(),
            plugins: dashmap::DashMap::new(),
            plugin_by_gts: dashmap::DashMap::new(),
        });

        self.services
            .set(services.clone())
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        info!("oagw gear initialized");
        Ok(())
    }
}

impl RestApiCapability for OagwGear {
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        let services = self
            .services
            .get()
            .ok_or_else(|| anyhow::anyhow!("oagw services not initialized"))?
            .clone();

        let router = crate::api::rest::routes::register_routes(router, openapi, services);
        info!("oagw REST routes registered");
        Ok(router)
    }
}
