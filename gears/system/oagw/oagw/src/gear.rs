//! ToolKit gear wiring for the Outbound API Gateway.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use credstore_sdk::CredStoreClientV1;
use tenant_resolver_sdk::TenantResolverClient;
use tokio_util::sync::CancellationToken;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::SystemCapability;
use toolkit::lifecycle::ReadySignal;
use toolkit::{Gear, GearCtx, RestApiCapability};
use tracing::info;
use types_registry_sdk::TypesRegistryClient;

use crate::api::rest::OagwState;
use crate::config::OagwConfig;
use crate::domain::plugin::PluginCatalog;
use crate::domain::repo::{
    PluginRepository, RouteRepository, TenantDirectory, UpstreamRepository,
};
use crate::domain::services::DataPlaneService;
use crate::domain::services::management::ControlPlaneService;
use crate::infra::metrics::OagwMetrics;
use crate::infra::plugin::{PluginRegistries, TokenCacheConfig};
use crate::infra::proxy::{DataPlaneServiceImpl, UpstreamConnector};
use crate::infra::ratelimit::RateLimiterRegistry;
use crate::infra::storage::{InMemoryPluginRepo, InMemoryRouteRepo, InMemoryUpstreamRepo};
use crate::infra::tenant_directory::{FlatTenantDirectory, TenantResolverDirectory};
use crate::infra::type_catalog;

/// Everything the gear assembles during `init`.
struct Wiring {
    control_plane: Arc<ControlPlaneService>,
    data_plane: Arc<dyn DataPlaneService>,
    limiter: Arc<RateLimiterRegistry>,
    config: OagwConfig,
}

#[toolkit::gear(
    name = "oagw",
    deps = [types_registry, authz_resolver, tenant_resolver, credstore],
    capabilities = [system, rest, stateful],
    lifecycle(entry = "serve", stop_timeout = "10s", await_ready)
)]
pub struct OagwGear {
    wiring: OnceLock<Arc<Wiring>>,
    types_registry: OnceLock<Arc<dyn TypesRegistryClient>>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            wiring: OnceLock::new(),
            types_registry: OnceLock::new(),
        }
    }
}

impl OagwGear {
    /// Background loop: garbage-collects unlinked custom plugins
    /// (DESIGN "Plugin Lifecycle Management").
    #[allow(
        clippy::redundant_pub_crate,
        reason = "module-private serve entry-point invoked by the toolkit runtime"
    )]
    pub(crate) async fn serve(
        self: Arc<Self>,
        cancel: CancellationToken,
        ready: ReadySignal,
    ) -> anyhow::Result<()> {
        let Some(wiring) = self.wiring.get().cloned() else {
            anyhow::bail!("oagw: serve invoked before init");
        };

        let tick = Duration::from_secs(wiring.config.plugin_gc_interval_secs.max(1));
        let mut interval = tokio::time::interval(tick);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick fires immediately; skip it so startup does no work.
        interval.tick().await;

        ready.notify();
        info!(
            target: "oagw.lifecycle",
            gc_interval_secs = tick.as_secs(),
            "oagw plugin garbage collector started"
        );

        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                _ = interval.tick() => {
                    match wiring
                        .control_plane
                        .run_plugin_gc(wiring.config.plugin_gc_ttl_days)
                        .await
                    {
                        Ok(0) => {}
                        Ok(deleted) => info!(
                            target: "oagw.lifecycle",
                            deleted,
                            "garbage-collected unlinked custom plugins"
                        ),
                        Err(err) => tracing::warn!(
                            target: "oagw.lifecycle",
                            error = %err,
                            "plugin garbage collection failed"
                        ),
                    }
                }
            }
        }

        info!(target: "oagw.lifecycle", "oagw plugin garbage collector stopped");
        Ok(())
    }
}

#[async_trait]
impl Gear for OagwGear {
    #[tracing::instrument(skip_all, fields(module = "oagw"))]
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let config: OagwConfig = ctx.config_or_default()?;
        config
            .validate()
            .map_err(|err| anyhow::anyhow!("oagw config invalid: {err}"))?;
        info!(
            allow_http_upstream = config.allow_http_upstream,
            ssrf_enabled = config.ssrf_policy.enabled,
            proxy_timeout_secs = config.proxy_timeout_secs,
            "initializing oagw module"
        );

        // OAGW is deployed without a `database:` block: configuration lives in
        // process, keyed exactly as the SQL schema would key it.
        let upstreams: Arc<dyn UpstreamRepository> = Arc::new(InMemoryUpstreamRepo::new());
        let routes: Arc<dyn RouteRepository> = Arc::new(InMemoryRouteRepo::new());
        let plugins: Arc<dyn PluginRepository> = Arc::new(InMemoryPluginRepo::new());

        let credstore = ctx
            .client_hub()
            .get::<dyn CredStoreClientV1>()
            .map_err(|e| anyhow::anyhow!("failed to get CredStoreClientV1: {e}"))?;

        // The tenant chain drives alias shadowing; without a resolver each
        // tenant is treated as its own root rather than failing the gear.
        let directory: Arc<dyn TenantDirectory> =
            match ctx.client_hub().get::<dyn TenantResolverClient>() {
                Ok(client) => Arc::new(TenantResolverDirectory::new(client, 300)),
                Err(err) => {
                    tracing::warn!(
                        target: "oagw.tenancy",
                        error = %err,
                        "tenant-resolver client unavailable; alias shadowing is disabled"
                    );
                    Arc::new(FlatTenantDirectory)
                }
            };

        let registries = Arc::new(PluginRegistries::with_builtins(
            credstore,
            TokenCacheConfig {
                ttl: config.token_cache_ttl(),
                capacity: config.token_cache_capacity,
            },
        ));
        let catalog: Arc<dyn PluginCatalog> = Arc::clone(&registries) as Arc<dyn PluginCatalog>;

        let control_plane = Arc::new(ControlPlaneService::new(
            upstreams,
            routes,
            plugins,
            directory,
            catalog,
        ));

        let metrics = Arc::new(OagwMetrics::from_global());
        let connector = Arc::new(UpstreamConnector::new(&config));
        let limiter = Arc::new(RateLimiterRegistry::new());
        let data_plane: Arc<dyn DataPlaneService> = Arc::new(DataPlaneServiceImpl::new(
            Arc::clone(&control_plane),
            Arc::clone(&registries),
            connector,
            Arc::clone(&limiter),
            metrics,
            config.clone(),
        ));

        if let Ok(registry) = ctx.client_hub().get::<dyn TypesRegistryClient>() {
            let _ = self.types_registry.set(registry);
        }

        self.wiring
            .set(Arc::new(Wiring {
                control_plane,
                data_plane,
                limiter,
                config,
            }))
            .map_err(|_| anyhow::anyhow!("{} module already initialized", Self::MODULE_NAME))?;

        info!("oagw module initialized");
        Ok(())
    }
}

#[async_trait]
impl SystemCapability for OagwGear {
    async fn post_init(&self, _sys: &toolkit::runtime::SystemContext) -> anyhow::Result<()> {
        // Type provisioning is best-effort: discovery degrades, traffic does not.
        if let Some(registry) = self.types_registry.get() {
            type_catalog::provision(registry).await;
        }
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
        let wiring = self
            .wiring
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("oagw services not initialized"))?;
        let state = Arc::new(OagwState {
            control_plane: Arc::clone(&wiring.control_plane),
            data_plane: Arc::clone(&wiring.data_plane),
            limiter: Arc::clone(&wiring.limiter),
        });
        let router = crate::api::rest::register_routes(router, openapi, state);
        info!("oagw REST routes registered");
        Ok(router)
    }
}
