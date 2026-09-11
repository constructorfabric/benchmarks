//! The `oagw` gear: wiring for the control plane, the data plane and the REST
//! surface.
//!
//! Configuration arrives under `gears.oagw.config` and is read with
//! [`GearCtx::config_or_default`], so an unconfigured deployment boots with the
//! documented defaults. The credential store and the tenant resolver are
//! optional collaborators: when the host did not register them, references
//! resolve to nothing and the scope is the caller's own tenant.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use credstore_sdk::CredStoreClientV1;
use toolkit::Gear;
use toolkit::GearCtx;
use toolkit::RestApiCapability;
use toolkit::SystemCapability;
use toolkit::api::OpenApiRegistry;
use tracing::info;

use crate::api::rest::routes;
use crate::config::OagwConfig;
use crate::domain::plugin::PluginCatalog;
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};
use crate::domain::services::control_plane::ControlPlaneService;
use crate::infra::memory_repo::{InMemoryPluginRepo, InMemoryRouteRepo, InMemoryUpstreamRepo};
use crate::infra::plugin::registry::{
    AuthPluginRegistry, GuardPluginRegistry, PluginCatalogView, TransformPluginRegistry,
};
use crate::infra::proxy::service::GatewayService;
use crate::infra::proxy::{credentials, outbound};
use crate::infra::tenant_scope::TenantScope;

/// The `oagw` gear.
#[toolkit::gear(
    name = "oagw",
    deps = [credstore, types_registry, tenant_resolver],
    capabilities = [system, rest]
)]
pub struct Oagw {
    config: OnceLock<OagwConfig>,
    control_plane: OnceLock<Arc<ControlPlaneService>>,
    gateway: OnceLock<Arc<GatewayService>>,
}

impl Default for Oagw {
    fn default() -> Self {
        Self {
            config: OnceLock::new(),
            control_plane: OnceLock::new(),
            gateway: OnceLock::new(),
        }
    }
}

#[async_trait]
impl Gear for Oagw {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let config: OagwConfig = ctx.config_or_default()?;
        config.validate()?;

        let upstreams = Arc::new(InMemoryUpstreamRepo::new());
        let routes = Arc::new(InMemoryRouteRepo::new());
        let bindings = Arc::new(InMemoryPluginRepo::new());
        let hierarchy = Arc::new(TenantScope::over(
            ctx.client_hub()
                .get::<dyn tenant_resolver_sdk::TenantResolverClient>()
                .map_err(|error| anyhow::anyhow!("oagw requires the tenant resolver: {error}"))?,
        ));
        let resolver = credential_resolver(ctx);
        let catalog = Arc::new(PluginCatalogView::new(
            Arc::new(AuthPluginRegistry::with_builtins()),
            Arc::new(GuardPluginRegistry::with_builtins()),
            Arc::new(TransformPluginRegistry::with_builtins()),
        ));

        let control_plane = Arc::new(ControlPlaneService::new(
            upstreams.clone(),
            routes.clone(),
            bindings,
            hierarchy.clone(),
            catalog,
        ));
        let gateway = Arc::new(GatewayService::new(
            routes,
            upstreams,
            hierarchy,
            Arc::new(crate::infra::proxy::rate_limit::TokenBucketRegistry::new()),
            resolver,
            Arc::new(AuthPluginRegistry::with_builtins()),
            Arc::new(GuardPluginRegistry::with_builtins()),
            Arc::new(TransformPluginRegistry::with_builtins()),
            config.allow_http_upstream,
            config.proxy_timeout_secs,
            config.max_body_bytes,
        ));

        let allow_http = config.allow_http_upstream;
        let proxy_timeout = config.proxy_timeout_secs;
        self.config
            .set(config)
            .map_err(|_| anyhow::anyhow!("{} already initialized", Self::MODULE_NAME))?;
        self.control_plane
            .set(control_plane)
            .map_err(|_| anyhow::anyhow!("{} already initialized", Self::MODULE_NAME))?;
        self.gateway
            .set(gateway)
            .map_err(|_| anyhow::anyhow!("{} already initialized", Self::MODULE_NAME))?;

        info!(
            allow_http_upstream = allow_http,
            proxy_timeout_secs = proxy_timeout,
            "oagw gear initialized"
        );
        Ok(())
    }
}

/// Build the credential resolver, or a failing one when the store is absent.
fn credential_resolver(
    ctx: &GearCtx,
) -> Arc<dyn crate::domain::services::data_plane::CredentialResolver> {
    let lookup = if let Ok(client) = ctx.client_hub().get::<dyn CredStoreClientV1>() {
        credentials::StoreLookup::new(client)
    } else {
        info!("no credential store registered; credential references resolve to nothing");
        credentials::StoreLookup::absent()
    };
    Arc::new(credentials::CredStoreResolver::new(Arc::new(lookup)))
}

impl SystemCapability for Oagw {}

impl RestApiCapability for Oagw {
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        let control_plane = self
            .control_plane
            .get()
            .ok_or_else(|| anyhow::anyhow!("oagw control plane not initialized"))?
            .clone();
        let gateway = self
            .gateway
            .get()
            .ok_or_else(|| anyhow::anyhow!("oagw data plane not initialized"))?
            .clone();
        info!("Registering oagw REST routes");
        Ok(routes::register_routes(
            router,
            openapi,
            control_plane,
            gateway,
        ))
    }
}

/// The configuration the gear booted with, for tests and diagnostics.
impl Oagw {
    /// The configuration, once `init` has run.
    #[must_use]
    pub fn config(&self) -> Option<&OagwConfig> {
        self.config.get()
    }

    /// The control plane, once `init` has run.
    #[must_use]
    pub fn control_plane(&self) -> Option<Arc<ControlPlaneService>> {
        self.control_plane.get().cloned()
    }

    /// The data plane, once `init` has run.
    #[must_use]
    pub fn gateway(&self) -> Option<Arc<GatewayService>> {
        self.gateway.get().cloned()
    }
}

/// Silence the unused-import lint for the repository traits, which the gear
/// wiring only mentions through `Arc::new`.
const _: Option<&dyn RouteRepository> = None;
const _: Option<&dyn UpstreamRepository> = None;
const _: Option<&dyn PluginRepository> = None;
const _: Option<&dyn PluginCatalog> = None;
const _: Option<&outbound::PoolSelector> = None;
