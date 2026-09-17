//! Gear declaration for the OAGW (outbound API gateway) gear.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use pingora_memory_cache::MemoryCache;
use tenant_resolver_sdk::TenantResolverClient;
use toolkit::{Gear, GearCtx, RestApiCapability};
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::SystemCapability;
use tracing::{debug, info};

use crate::config::OagwConfig;
use crate::domain::service::{ControlPlaneService, DataPlaneService};
use crate::infra::plugins::{CachedToken, PluginRegistryImpl};
use crate::infra::proxy::DataPlaneServiceImpl;
use crate::infra::storage::{InMemoryPluginRepo, InMemoryRouteRepo, InMemoryUpstreamRepo};

/// OAGW — outbound API gateway gear.
///
/// Provides:
///
/// - **Control plane**: upstream / route / plugin CRUD at `/oagw/v1/*`.
/// - **Data plane**: request proxying at `/oagw/v1/proxy/{alias}/{*path}`.
///
/// ## Capabilities
///
/// - `system` — core infrastructure gear, initialized early (needs the
///   credstore / tenant-resolver clients published by sibling gears).
/// - `rest` — exposes the management + proxy surface.
///
/// ## Registration
///
/// Registered via `#[toolkit::gear]`; the example server's
/// `registered_gears.rs` links this crate (`use api_egress as _;`). The host
/// (api-gateway) nests this gear's router under its configured `prefix_path`
/// (empty in the e2e setup), yielding routes at `/oagw/v1/...`.
#[toolkit::gear(
    name = "oagw",
    capabilities = [system, rest],
    deps = [credstore, tenant_resolver]
)]
pub struct OagwGear {
    control: OnceLock<Arc<ControlPlaneService>>,
    data_plane: OnceLock<Arc<dyn DataPlaneService>>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            control: OnceLock::new(),
            data_plane: OnceLock::new(),
        }
    }
}

#[async_trait]
impl Gear for OagwGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: OagwConfig = ctx.config_or_default()?;
        debug!(
            proxy_timeout_secs = cfg.proxy_timeout_secs,
            allow_http_upstream = cfg.allow_http_upstream,
            ssrf_policy_enabled = cfg.ssrf_policy.enabled,
            token_cache = ?cfg.token_cache,
            "oagw: loaded configuration"
        );

        // Shared in-memory stores.
        let upstreams = Arc::new(InMemoryUpstreamRepo::new());
        let routes = Arc::new(InMemoryRouteRepo::new());
        let plugins = Arc::new(InMemoryPluginRepo::new());

        // Control plane.
        let control = Arc::new(ControlPlaneService::new(
            upstreams.clone(),
            routes.clone(),
            plugins.clone(),
            cfg.clone(),
        ));

        // Data plane dependencies: credential store + tenant resolver clients
        // published by sibling gears (credstore / tenant-resolver).
        let credstore = ctx.client_hub().get::<dyn credstore_sdk::CredStoreClientV1>()?;
        let tenants = ctx.client_hub().get::<dyn TenantResolverClient>()?;

        let token_cache: Arc<MemoryCache<String, CachedToken>> =
            Arc::new(MemoryCache::new(cfg.token_cache.cache_capacity));

        let plugin_registry = Arc::new(PluginRegistryImpl::new(
            credstore,
            token_cache,
            cfg.token_cache.clone(),
            Duration::from_secs(cfg.proxy_timeout_secs.max(1)),
            plugins.clone(),
        ));

        let data_plane: Arc<dyn DataPlaneService> = Arc::new(DataPlaneServiceImpl::new(
            upstreams,
            routes,
            plugin_registry,
            tenants,
            cfg,
        )?);

        self.control
            .set(control)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
        self.data_plane
            .set(data_plane)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        info!("oagw: gear initialized");
        Ok(())
    }
}

#[async_trait]
impl SystemCapability for OagwGear {
    async fn post_init(&self, _sys: &toolkit::runtime::SystemContext) -> anyhow::Result<()> {
        info!("oagw: post_init complete (all stores in-memory)");
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

        let control = self
            .control
            .get()
            .ok_or_else(|| anyhow::anyhow!("Control plane service not initialized"))?
            .clone();
        let data_plane = self
            .data_plane
            .get()
            .ok_or_else(|| anyhow::anyhow!("Data plane not initialized"))?
            .clone();

        let router = crate::api::rest::routes::register_routes(router, openapi, control, data_plane);
        info!("oagw REST routes registered successfully");
        Ok(router)
    }
}
