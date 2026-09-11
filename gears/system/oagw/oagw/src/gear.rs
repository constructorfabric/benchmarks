//! Gear declaration for the oagw gear.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use credstore_sdk::CredStoreClientV1;
use toolkit::api::OpenApiRegistry;
use toolkit::{Gear, GearCtx, RestApiCapability};
use tracing::{debug, info};
use types_registry_sdk::TypesRegistryClient;

use crate::config::OagwConfig;
use crate::domain::plugin::registry::{
    AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry,
};
use crate::domain::ratelimit::RateLimiter;
use crate::domain::services::ControlPlaneService;
use crate::domain::services::proxy::DataPlane;
use crate::infra::credstore::CredStoreResolver;
use crate::infra::memory_repo::{MemoryPluginRepo, MemoryRouteRepo, MemoryUpstreamRepo};
use crate::infra::metrics::Metrics;
use crate::infra::proxy::outbound::OutboundClient;
use crate::infra::proxy::service::DataPlaneServiceImpl;
use crate::infra::type_provisioning;

/// Outbound API gateway gear.
///
/// Terminates the management REST API under `/oagw/v1/...` and the proxy under
/// `/oagw/v1/proxy/{alias}`. Owns no database: its repositories are in memory
/// and its configuration comes from `oagw.config`.
///
/// ## Capabilities
///
/// - `rest` — Exposes the management and proxy endpoints.
#[toolkit::gear(
    name = "oagw",
    capabilities = [rest]
)]
pub struct OagwGear {
    control_plane: OnceLock<Arc<ControlPlaneService>>,
    proxy_state: OnceLock<Arc<crate::api::rest::handlers::proxy::ProxyState>>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            control_plane: OnceLock::new(),
            proxy_state: OnceLock::new(),
        }
    }
}

impl OagwGear {
    /// The control plane, once initialised.
    #[must_use]
    pub fn control_plane(&self) -> Option<&Arc<ControlPlaneService>> {
        self.control_plane.get()
    }
}

#[async_trait]
impl Gear for OagwGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: OagwConfig = ctx.config_or_default()?;
        cfg.validate()
            .map_err(|e| anyhow::anyhow!("oagw config: {e}"))?;
        debug!(
            proxy_timeout_secs = cfg.proxy_timeout_secs,
            allow_http_upstream = cfg.allow_http_upstream,
            ssrf_policy_enabled = cfg.ssrf_policy.enabled,
            "Loaded oagw config"
        );

        let upstreams: Arc<MemoryUpstreamRepo> = Arc::new(MemoryUpstreamRepo::new());
        let routes: Arc<MemoryRouteRepo> = Arc::new(MemoryRouteRepo::new());
        let plugins: Arc<MemoryPluginRepo> = Arc::new(MemoryPluginRepo::new());
        let control_plane = Arc::new(ControlPlaneService::new(
            upstreams.clone(),
            routes.clone(),
            plugins.clone(),
        ));

        // The credential store is optional at start-up: the gear fails closed
        // per credential reference until the client appears in the hub.
        let credentials: Arc<CredStoreResolver> = Arc::new(CredStoreResolver::new(
            ctx.client_hub().get::<dyn CredStoreClientV1>().ok(),
        ));

        let outbound = OutboundClient::new(cfg.proxy_timeout(), cfg.permits_plaintext_connection());
        let token_fetcher: Arc<dyn crate::domain::plugin::oauth2_client_cred::TokenFetcher> =
            Arc::new(crate::infra::proxy::token_fetcher::HttpTokenFetcher::new(
                outbound.clone(),
            ));
        let auth_plugins = AuthPluginRegistry::with_builtins(&token_fetcher);
        let guard_plugins = GuardPluginRegistry::with_builtins();
        let transform_plugins = TransformPluginRegistry::with_builtins();

        let metrics = Arc::new(Metrics::new());
        let limiter = Arc::new(RateLimiter::new());

        let plane = Arc::new(DataPlaneServiceImpl::new(
            upstreams,
            routes,
            credentials,
            auth_plugins,
            guard_plugins,
            transform_plugins,
            limiter,
            outbound,
            metrics.clone(),
        ));

        let plane: Arc<dyn DataPlane> = plane;
        let proxy_state = Arc::new(crate::api::rest::handlers::proxy::ProxyState {
            plane,
            metrics,
            anonymous_tenant: uuid::Uuid::nil(),
        });

        self.control_plane
            .set(control_plane)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
        self.proxy_state
            .set(proxy_state)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        // Publish the owned types so other gears can reference them.
        let registry_client = ctx.client_hub().get::<dyn TypesRegistryClient>().ok();
        if let Some(registry) = registry_client {
            match type_provisioning::provision(&registry).await {
                Ok(count) => info!(count, "Registered oagw type schemas"),
                Err(error) => tracing::warn!(error = %error, "oagw type registration skipped"),
            }
        } else {
            debug!("types-registry client unavailable; oagw types not published");
        }

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
        info!("Registering oagw REST routes");

        let control_plane = self
            .control_plane
            .get()
            .ok_or_else(|| anyhow::anyhow!("oagw control plane not initialized"))?
            .clone();
        let proxy_state = self
            .proxy_state
            .get()
            .ok_or_else(|| anyhow::anyhow!("oagw proxy state not initialized"))?
            .clone();

        let router =
            crate::api::rest::routes::register_routes(router, openapi, control_plane, proxy_state);

        info!("oagw REST routes registered successfully");
        Ok(router)
    }
}
