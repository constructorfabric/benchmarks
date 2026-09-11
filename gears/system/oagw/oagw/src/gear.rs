//! Gear declaration for the OAGW (outbound `API` gateway) gear.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use toolkit::api::OpenApiRegistry;
use toolkit::{Gear, GearCtx, RestApiCapability};
use tracing::info;

use crate::api::rest::state::{OagwState, build_registries};
use crate::config::OagwConfig;
use crate::infra::control_plane::OagwControlPlane;
use crate::infra::memory::{
    MemoryPluginRepository, MemoryRouteRepository, MemoryUpstreamRepository,
};
use crate::infra::proxy::ProxyEngine;

/// OAGW gear.
///
/// Publishes the outbound-`API`-gateway control plane (upstreams, routes,
/// plugins) and the data plane that forwards proxied requests.
///
/// ## Capabilities
///
/// - `rest` — exposes the management and proxy endpoints
#[toolkit::gear(
    name = "oagw",
    deps = [types_registry, tenant_resolver, credstore],
    capabilities = [rest]
)]
pub struct OagwGear {
    state: OnceLock<Arc<OagwState>>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            state: OnceLock::new(),
        }
    }
}

impl OagwGear {
    /// The REST state, after `init`.
    ///
    /// # Errors
    ///
    /// Returns an error when the gear has not been initialised.
    pub fn rest_state(&self) -> anyhow::Result<Arc<OagwState>> {
        self.state
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("{} gear not initialized", Self::MODULE_NAME))
    }
}

#[async_trait]
impl Gear for OagwGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let config: OagwConfig = ctx.config_or_default()?;
        info!(
            allow_http_upstream = config.allow_http_upstream,
            proxy_timeout_secs = config.proxy_timeout_secs,
            token_cache_capacity = config.token_cache_capacity,
            "Loaded oagw config"
        );

        let registries = build_registries(ctx, &config)?;
        let tenant_client: Option<Arc<dyn tenant_resolver_sdk::TenantResolverClient>> = ctx
            .client_hub()
            .get::<dyn tenant_resolver_sdk::TenantResolverClient>()
            .ok();

        let control_plane = Arc::new(OagwControlPlane::new(
            Arc::new(MemoryUpstreamRepository::default()),
            Arc::new(MemoryRouteRepository::default()),
            Arc::new(MemoryPluginRepository::default()),
            Arc::clone(&registries),
            tenant_client,
            config.clone(),
        ));
        let engine = Arc::new(ProxyEngine::new(
            control_plane.clone(),
            Arc::clone(&registries),
            config,
        ));

        self.state
            .set(Arc::new(OagwState {
                control_plane,
                engine,
                registries,
            }))
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

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
        let state = self.rest_state()?;

        let router = crate::api::rest::routes::register(router, openapi);
        let router = crate::api::rest::proxy::register(router, openapi);
        let router = router.layer(axum::Extension(state));

        info!("OAGW REST routes registered successfully");
        Ok(router)
    }
}
