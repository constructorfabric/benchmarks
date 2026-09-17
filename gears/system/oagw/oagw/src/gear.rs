//! Gear declaration for the `oagw` outbound API gateway.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use tenant_resolver_sdk::TenantResolverClient;
use toolkit::api::OpenApiRegistry;
use toolkit::{Gear, GearCtx, RestApiCapability};
use tracing::info;

use crate::config::OagwConfig;
use crate::domain::services::OagwService;

/// The outbound API gateway gear.
///
/// Registers the management API (`/oagw/v1/upstreams`, `/oagw/v1/routes`,
/// `/oagw/v1/plugins`) and the data plane (`/oagw/v1/proxy/{alias}/{*path}`)
/// under the `rest` capability.
#[toolkit::gear(
    name = "oagw",
    capabilities = [rest]
)]
pub struct OagwGear {
    service: OnceLock<Arc<OagwService>>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            service: OnceLock::new(),
        }
    }
}

impl OagwGear {
    /// The composed service, available after `init`.
    ///
    /// # Errors
    /// Returns an error when the gear has not been initialized yet.
    pub fn service(&self) -> anyhow::Result<Arc<OagwService>> {
        self.service
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("oagw gear not initialized"))
    }
}

#[async_trait]
impl Gear for OagwGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: OagwConfig = ctx.config_or_default()?;
        info!(
            proxy_timeout_secs = cfg.proxy_timeout_secs,
            allow_http_upstream = cfg.allow_http_upstream,
            "Loaded oagw config"
        );

        let resolver = ctx.client_hub().get::<dyn TenantResolverClient>()?;
        let hierarchy = crate::infra::hierarchy::ResolverTenantHierarchy::new(resolver);
        let service = Arc::new(OagwService::new(cfg, Arc::new(hierarchy)));
        self.service
            .set(service)
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
        let service = self.service()?;
        let router = crate::api::rest::routes::register_routes(router, openapi, service)?;
        info!("oagw REST routes registered successfully");
        Ok(router)
    }
}
