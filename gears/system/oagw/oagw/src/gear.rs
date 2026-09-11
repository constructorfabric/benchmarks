//! The `ToolKit` gear: wiring, initialization and REST registration.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use toolkit::api::OpenApiRegistry;
use toolkit::{Gear, GearCtx, RestApiCapability};

use crate::api::rest::routes;
use crate::config::OagwConfig;
use crate::domain::services::management::ControlPlaneService;
use crate::infra::plugin::registry::{
    AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry,
};
use crate::infra::proxy::service::DataPlaneService;
use crate::infra::storage::memory::{
    MemoryPluginRepository, MemoryRouteRepository, MemoryUpstreamRepository, TenantHierarchyClient,
};

/// The OAGW gear: an outbound API gateway over `cred_store`-backed upstreams.
///
/// The control plane owns the upstream/route/plugin tables; the data plane
/// executes proxy requests against them. Both live in
/// `gears.oagw.config`-driven instances created once at init.
#[toolkit::gear(
    name = "oagw",
    deps = [credstore, types_registry],
    capabilities = [rest]
)]
#[derive(Default)]
pub struct OagwGear {
    control_plane: OnceLock<Arc<ControlPlaneService>>,
    data_plane: OnceLock<Arc<DataPlaneService>>,
}

impl OagwGear {
    /// The control plane, once initialized.
    #[must_use]
    pub fn control_plane(&self) -> Option<Arc<ControlPlaneService>> {
        self.control_plane.get().cloned()
    }

    /// The data plane, once initialized.
    #[must_use]
    pub fn data_plane(&self) -> Option<Arc<DataPlaneService>> {
        self.data_plane.get().cloned()
    }
}

#[async_trait]
impl Gear for OagwGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let config: OagwConfig = ctx
            .config_or_default()
            .map_err(|e| anyhow::anyhow!("oagw config is invalid: {e}"))?;

        let credstore: Arc<dyn credstore_sdk::CredStoreClientV1> = ctx
            .client_hub()
            .get::<dyn credstore_sdk::CredStoreClientV1>()
            .map_err(|e| anyhow::anyhow!("oagw requires the credstore client: {e}"))?;

        let hierarchy = Arc::new(TenantHierarchyClient::new(
            ctx.client_hub()
                .get::<dyn tenant_resolver_sdk::TenantResolverClient>()
                .ok(),
        ));

        let control_plane = Arc::new(ControlPlaneService::new(
            Arc::new(MemoryUpstreamRepository::default()),
            Arc::new(MemoryRouteRepository::default()),
            Arc::new(MemoryPluginRepository::default()),
            hierarchy,
            config.allow_http_upstream,
        ));

        let auth = AuthPluginRegistry::with_builtins(&credstore, &config);
        let guards = GuardPluginRegistry::with_builtins();
        let transforms = TransformPluginRegistry::with_builtins();

        let data_plane = Arc::new(
            DataPlaneService::new(control_plane.clone(), config.clone())
                .map_err(|e| anyhow::anyhow!("oagw data plane could not start: {e}"))?
                .with_registries(auth, guards, transforms),
        );

        self.control_plane
            .set(control_plane)
            .map_err(|_| anyhow::anyhow!("oagw gear already initialized"))?;
        self.data_plane
            .set(data_plane)
            .map_err(|_| anyhow::anyhow!("oagw gear already initialized"))?;

        tracing::info!(
            proxy_timeout_secs = config.proxy_timeout_secs,
            allow_http_upstream = config.allow_http_upstream,
            "OAGW gear initialized"
        );
        Ok(())
    }
}

impl RestApiCapability for OagwGear {
    fn register_rest(
        &self,
        ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        let control_plane = self
            .control_plane()
            .ok_or_else(|| anyhow::anyhow!("oagw control plane is not initialized"))?;
        let data_plane = self
            .data_plane()
            .ok_or_else(|| anyhow::anyhow!("oagw data plane is not initialized"))?;
        let config: OagwConfig = ctx
            .config_or_default()
            .map_err(|e| anyhow::anyhow!("oagw config is invalid: {e}"))?;
        tracing::info!("Registering OAGW REST routes");
        let router = routes::register_routes(router, openapi, control_plane, data_plane, config);
        tracing::info!("OAGW REST routes registered");
        Ok(router)
    }
}
