//! ToolKit gear wiring.
//!
//! OAGW is a single gear with an internal Control Plane / Data Plane split
//! (`cpt-cf-oagw-design-overview`). `init` builds both and registers the REST
//! surface, and `post_init` publishes the GTS type catalogue. Plugin
//! garbage collection is reconciled on configuration writes rather than from
//! a background tick, so there is no long-running task to supervise.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::SystemCapability;
use toolkit::{Gear, GearCtx, RestApiCapability};
use tracing::info;

use credstore_sdk::CredStoreClientV1;
use tenant_resolver_sdk::TenantResolverClient;
use types_registry_sdk::TypesRegistryClient;

use crate::api::rest::handlers::AppState;
use crate::config::OagwConfig;
use crate::domain::plugin::PluginCatalog;
use crate::domain::repo::{
    PluginRepository, PluginUsageRepository, RouteRepository, UpstreamRepository,
};
use crate::domain::services::management::ControlPlane;
use crate::domain::tenant::TenantChain;
use crate::infra::metrics::OagwMetrics;
use crate::infra::plugin::registry::PluginRegistries;
use crate::infra::proxy::DataPlane;
use crate::infra::storage::{
    MemoryPluginRepo, MemoryPluginUsageRepo, MemoryRouteRepo, MemoryStore, MemoryUpstreamRepo,
};
use crate::infra::tenant_dir::TenantResolverChain;
use crate::infra::type_catalog;

/// Seconds in a day, for turning the configured GC TTL into a deadline.
const SECS_PER_DAY: u64 = 86_400;
/// Ancestor-chain cache lifetime, in seconds.
const TENANT_CACHE_TTL_SECS: u64 = 30;

/// Everything `init` builds, kept for the later lifecycle phases.
struct Wiring {
    state: Arc<AppState>,
    types_registry: Arc<dyn TypesRegistryClient>,
}

/// The OAGW gear.
#[toolkit::gear(
    name = "oagw",
    deps = [authz_resolver, credstore, tenant_resolver, types_registry],
    capabilities = [system, rest]
)]
pub struct OagwGear {
    wiring: OnceLock<Wiring>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            wiring: OnceLock::new(),
        }
    }
}

#[async_trait]
impl Gear for OagwGear {
    #[tracing::instrument(skip_all, fields(module = "oagw"))]
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let mut config: OagwConfig = ctx.config_or_default()?;
        config
            .validate()
            .map_err(|err| anyhow::anyhow!("oagw config invalid: {err}"))?;
        if config.allow_http_upstream {
            tracing::warn!(
                target: "oagw.config",
                "oagw.config.allow_http_upstream is enabled: plaintext upstream connections \
                 are permitted. Do not enable this in production without compensating controls."
            );
        }
        if !config.ssrf_policy.enabled {
            tracing::warn!(
                target: "oagw.config",
                "oagw.config.ssrf_policy.enabled is false: resolved upstream addresses are not \
                 checked against internal network ranges."
            );
        }
        let config = Arc::new(config);

        // Fail-closed on the dependencies the request path cannot work
        // without: without CredStore no auth plugin can resolve a credential,
        // and both gears are hard `deps` so they initialise first.
        let credstore = ctx
            .client_hub()
            .get::<dyn CredStoreClientV1>()
            .map_err(|err| anyhow::anyhow!("failed to get CredStoreClientV1: {err}"))?;
        let tenant_resolver = ctx
            .client_hub()
            .get::<dyn TenantResolverClient>()
            .map_err(|err| anyhow::anyhow!("failed to get TenantResolverClient: {err}"))?;
        let types_registry = ctx
            .client_hub()
            .get::<dyn TypesRegistryClient>()
            .map_err(|err| anyhow::anyhow!("failed to get TypesRegistryClient: {err}"))?;

        let store = Arc::new(MemoryStore::new());
        let upstreams: Arc<dyn UpstreamRepository> =
            Arc::new(MemoryUpstreamRepo::new(Arc::clone(&store)));
        let routes: Arc<dyn RouteRepository> = Arc::new(MemoryRouteRepo::new(Arc::clone(&store)));
        let plugins: Arc<dyn PluginRepository> =
            Arc::new(MemoryPluginRepo::new(Arc::clone(&store)));
        let usage: Arc<dyn PluginUsageRepository> =
            Arc::new(MemoryPluginUsageRepo::new(Arc::clone(&store)));

        let tenants: Arc<dyn TenantChain> = Arc::new(TenantResolverChain::new(
            tenant_resolver,
            TENANT_CACHE_TTL_SECS,
        ));

        let registries = Arc::new(PluginRegistries::with_builtins(
            credstore,
            None,
            config.token_cache(),
        ));
        let catalog: Arc<dyn PluginCatalog> = Arc::clone(&registries) as Arc<dyn PluginCatalog>;

        let control = Arc::new(ControlPlane::new(
            upstreams,
            routes,
            plugins,
            usage,
            tenants,
            catalog,
            config.plugin_gc_ttl_days.saturating_mul(SECS_PER_DAY),
        ));
        let metrics = Arc::new(OagwMetrics::from_global());
        let data_plane = Arc::new(DataPlane::new(
            Arc::clone(&control),
            Arc::clone(&registries),
            metrics,
            Arc::clone(&config),
        ));

        let state = Arc::new(AppState {
            control,
            data_plane,
            max_body_bytes: config.max_body_bytes,
        });

        self.wiring
            .set(Wiring {
                state,
                types_registry,
            })
            .map_err(|_| anyhow::anyhow!("{} module already initialized", Self::MODULE_NAME))?;

        info!(
            proxy_timeout_secs = config.proxy_timeout_secs,
            allow_http_upstream = config.allow_http_upstream,
            "oagw module initialized"
        );
        Ok(())
    }
}

#[async_trait]
impl SystemCapability for OagwGear {
    async fn post_init(&self, _sys: &toolkit::runtime::SystemContext) -> anyhow::Result<()> {
        let Some(wiring) = self.wiring.get() else {
            return Ok(());
        };
        type_catalog::provision(&wiring.types_registry).await;
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
        let state = self
            .wiring
            .get()
            .map(|wiring| Arc::clone(&wiring.state))
            .ok_or_else(|| anyhow::anyhow!("oagw AppState not initialized"))?;
        let router = crate::api::rest::register_routes(router, openapi, state);
        info!("oagw REST routes registered under /oagw/v1");
        Ok(router)
    }
}
