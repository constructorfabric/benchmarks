//! OAGW gear entry point: service wiring + REST registration.
//!
//! The gear owns one in-memory store backing both the management
//! ([`ControlPlaneService`]) and data ([`DataPlaneService`]) planes, and
//! publishes its GTS type-schema/instance catalogue into the types-registry
//! at startup (see [`crate::type_catalog`]). Multi-tenancy is enforced
//! through the `AuthZ` PEP with the `TenantHierarchy` capability enabled.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use authz_resolver_sdk::{AuthZResolverClient, Capability, PolicyEnforcer};
use credstore_sdk::CredStoreClientV1;
use tenant_resolver_sdk::TenantResolverClient;
use toolkit::api::OpenApiRegistry;
use toolkit::{Gear, GearCtx, RestApiCapability};
use toolkit_http::{HttpClientBuilder, HttpClientConfig};
use tracing::info;
use types_registry_sdk::TypesRegistryClient;

use crate::api::rest::routes;
use crate::config::OagwConfig;
use crate::domain::repo::{PluginRepo, RouteRepo, UpstreamRepo};
use crate::domain::services::control_plane::ControlPlaneService;
use crate::domain::services::data_plane::DataPlaneService;
use crate::infra::plugins::PluginRegistry;
use crate::infra::proxy::ProxyService;
use crate::infra::ratelimit::RateLimiter;
use crate::infra::storage::MemoryStore;

/// Default number of distinct rate-limit buckets the in-memory limiter
/// accepts before evicting least-recently-used entries.
const RATE_LIMITER_BUCKETS: usize = 10_000;

/// Main gear struct for the OAGW gear.
#[toolkit::gear(
    name = "oagw",
    deps = [credstore, tenant_resolver, authz_resolver, types_registry],
    capabilities = [rest]
)]
#[allow(clippy::struct_field_names)]
pub struct Oagw {
    control_plane: OnceLock<Arc<ControlPlaneService>>,
    proxy: OnceLock<Arc<ProxyService>>,
}

impl Default for Oagw {
    fn default() -> Self {
        Self {
            control_plane: OnceLock::new(),
            proxy: OnceLock::new(),
        }
    }
}

#[async_trait]
impl Gear for Oagw {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let config: OagwConfig = ctx.config_or_default().map_err(|e| {
            anyhow::anyhow!("{} gear config: {e}", Self::MODULE_NAME)
        })?;

        // One in-memory store backs both planes (single-node prototype).
        let store = Arc::new(MemoryStore::new());
        let upstreams: Arc<dyn UpstreamRepo> = Arc::new(store.clone().upstreams());
        let routes: Arc<dyn RouteRepo> = Arc::new(store.clone().routes());
        let plugins: Arc<dyn PluginRepo> = Arc::new(store.clone().plugins());
        // AuthZ PEP with tenant-hierarchy scoping (multi-tenant management).
        let authz = ctx
            .client_hub()
            .get::<dyn AuthZResolverClient>()
            .map_err(|e| anyhow::anyhow!("failed to get AuthZ resolver: {e}"))?;
        let enforcer = Arc::new(
            PolicyEnforcer::new(authz).with_capabilities(vec![Capability::TenantHierarchy]),
        );
        let tenant_resolver = ctx
            .client_hub()
            .get::<dyn TenantResolverClient>()
            .map_err(|e| anyhow::anyhow!("failed to get Tenant resolver: {e}"))?;

        // Publish the GTS catalogue (best-effort — see `type_catalog`).
        let types_registry = ctx
            .client_hub()
            .get::<dyn TypesRegistryClient>()
            .map_err(|e| anyhow::anyhow!("failed to get TypesRegistryClient: {e}"))?;
        crate::type_catalog::register_catalog(types_registry.as_ref()).await?;

        let control_plane = Arc::new(ControlPlaneService::new(
            upstreams.clone(),
            routes.clone(),
            plugins.clone(),
            enforcer.clone(),
            tenant_resolver.clone(),
            config.allow_http_upstream,
        ));
        self.control_plane
            .set(control_plane)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        let data_plane = Arc::new(DataPlaneService::new(
            upstreams,
            routes,
            enforcer,
            tenant_resolver,
        ));

        let plugin_registry = Arc::new(PluginRegistry::with_builtins(
            Duration::from_secs(config.token_cache_ttl_secs),
            config.token_cache_capacity,
            plugins,
        ));

        let cred_store = ctx
            .client_hub()
            .get::<dyn CredStoreClientV1>()
            .map_err(|e| anyhow::anyhow!("failed to get CredStore client: {e}"))?;

        let http = HttpClientBuilder::with_config(HttpClientConfig::proxy())
            .timeout(Duration::from_secs(config.proxy_timeout_secs))
            .build()
            .map_err(|e| anyhow::anyhow!("failed to build OAGW http client: {e}"))?;

        let proxy = Arc::new(ProxyService::new(
            data_plane,
            plugin_registry,
            Arc::new(RateLimiter::with_capacity(RATE_LIMITER_BUCKETS)),
            cred_store,
            http,
            Arc::new(config),
        ));
        self.proxy
            .set(proxy)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        info!("{} gear initialized (control plane + data plane)", Self::MODULE_NAME);
        Ok(())
    }
}

impl RestApiCapability for Oagw {
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        info!("Registering OAGW REST routes");

        let control = self
            .control_plane
            .get()
            .ok_or_else(|| anyhow::anyhow!("OAGW control plane not initialized"))?
            .clone();
        let proxy = self
            .proxy
            .get()
            .ok_or_else(|| anyhow::anyhow!("OAGW proxy not initialized"))?
            .clone();

        let router = routes::register_routes(router, openapi, control, proxy);
        info!("OAGW REST routes registered successfully");
        Ok(router)
    }
}
