//! Gear declaration for the `oagw` outbound API gateway.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::SystemCapability;
use toolkit::{Gear, GearCtx, RestApiCapability};
use tracing::{debug, info};

use crate::config::OagwConfig;
use crate::infra::plugin::registry::{
    AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry,
};
use crate::infra::plugin::secret_store::HubSecretResolver;
use crate::infra::proxy::service::DataPlaneServiceImpl;
use crate::infra::proxy::tenant::{HubTenantHierarchy, TenantHierarchy};

/// The outbound API gateway gear.
///
/// ## Capabilities
///
/// - `system` — owns its control-plane state, initialized early in start-up
/// - `rest` — serves the management API and the proxy
///
/// ## Surfaces
///
/// Both are mounted gear-relative:
///
/// - management: `/oagw/v1/{upstreams,routes,plugins}`
/// - proxy: `/oagw/v1/proxy/{alias}[/{*path}]`
#[toolkit::gear(
    name = "oagw",
    capabilities = [system, rest]
)]
pub struct OagwGear {
    control_plane: OnceLock<Arc<dyn crate::domain::services::management::ControlPlane>>,
    data_plane: OnceLock<Arc<DataPlaneServiceImpl>>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            control_plane: OnceLock::new(),
            data_plane: OnceLock::new(),
        }
    }
}

impl OagwGear {
    /// The control plane, once the gear is initialized.
    #[must_use]
    pub fn control_plane(&self) -> Option<Arc<dyn crate::domain::services::management::ControlPlane>> {
        self.control_plane.get().cloned()
    }

    /// The data plane, once the gear is initialized.
    #[must_use]
    pub fn data_plane(&self) -> Option<Arc<DataPlaneServiceImpl>> {
        self.data_plane.get().cloned()
    }
}

#[async_trait]
impl Gear for OagwGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let config: OagwConfig = ctx.config_or_default()?;
        config.validate()?;
        debug!(
            proxy_timeout_secs = config.proxy_timeout_secs,
            connect_timeout_secs = config.connect_timeout_secs,
            allow_http_upstream = config.allow_http_upstream,
            token_cache_ttl_secs = config.token_cache_ttl_secs,
            "Loaded oagw config"
        );
        debug!("{}", crate::infra::type_provisioning::describe());

        let control_plane = Arc::new(crate::infra::storage::memory::in_memory_control_plane());
        let (upstreams, routes, plugins) = control_plane.repositories();

        let tenants = tenant_hierarchy(ctx);
        let secrets = secret_resolver(ctx);
        let auth_plugins = AuthPluginRegistry::with_builtins(
            secrets,
            std::time::Duration::from_secs(config.token_cache_ttl_secs),
            config.token_cache_capacity,
        );

        let data_plane = Arc::new(DataPlaneServiceImpl::new(
            upstreams,
            routes,
            plugins,
            auth_plugins,
            GuardPluginRegistry::with_builtins(),
            TransformPluginRegistry::with_builtins(),
            tenants,
            config,
        ));

        let plane: Arc<dyn crate::domain::services::management::ControlPlane> = control_plane;
        self.control_plane
            .set(plane)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
        self.data_plane
            .set(data_plane)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        info!("oagw gear initialized: management and proxy surfaces ready");
        Ok(())
    }
}

/// The tenant hierarchy, read through the client hub.
///
/// This gear initializes before the tenant resolver does, so the client is
/// probed per lookup rather than captured here: capturing it at init would
/// freeze a `None` that never recovers.
fn tenant_hierarchy(ctx: &GearCtx) -> Arc<dyn TenantHierarchy> {
    debug!("oagw reads the tenant hierarchy through the client hub");
    Arc::new(HubTenantHierarchy::new(ctx.client_hub()))
}

/// The secret resolver, read through the client hub.
///
/// As with [`tenant_hierarchy`], the credential store registers its client
/// after this gear initializes, so the resolver probes the hub at resolve time.
fn secret_resolver(ctx: &GearCtx) -> Arc<HubSecretResolver> {
    debug!("oagw resolves credentials through the client hub");
    Arc::new(HubSecretResolver::new(ctx.client_hub()))
}

// System gear: oagw owns its control-plane state and needs no pre/post-init
// work beyond what `init` already does; the capability buys the early ordering.
impl SystemCapability for OagwGear {}

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
        let data_plane = self
            .data_plane
            .get()
            .ok_or_else(|| anyhow::anyhow!("oagw data plane not initialized"))?
            .clone();

        let router =
            crate::api::rest::routes::register_routes(router, openapi, control_plane, data_plane)?;

        info!("oagw REST routes registered successfully");
        Ok(router)
    }
}
