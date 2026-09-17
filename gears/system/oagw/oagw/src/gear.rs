//! Gear declaration for the OAGW (outbound API gateway) gear.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use toolkit::api::OpenApiRegistry;
use toolkit::{Gear, GearCtx, RestApiCapability};
use tracing::{debug, info};

use crate::config::OagwConfig;
use crate::domain::service::OagwService;

/// OAGW gear: manages upstreams/routes/plugins and proxies outbound
/// requests through the api-gateway's authenticated route surface.
///
/// ## Capabilities
///
/// - `rest` — Exposes the management + proxy REST API
///
/// The gear owns a single [`OagwService`] (in-memory upstream/route/plugin
/// registry + proxy data plane). Once `init` completes, the service is made
/// available through the ClientHub-less path: REST handlers receive it via
/// an axum `Extension`.
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

#[async_trait]
impl Gear for OagwGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: OagwConfig = ctx.config_or_default()?;
        debug!(
            proxy_timeout_secs = cfg.proxy_timeout_secs,
            allow_http_upstream = cfg.allow_http_upstream,
            "Loaded oagw config"
        );

        let client = toolkit_http::HttpClient::builder()
            .timeout(cfg.proxy_timeout())
            .build()
            .map_err(|e| anyhow::anyhow!("oagw: failed to build outbound HTTP client: {e}"))?;

        let service = Arc::new(OagwService::new(cfg, client));
        self.service
            .set(service.clone())
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        // Wire the ClientHub so authentication plugins can resolve
        // credentials from the credstore at proxy time.
        service.attach_client_hub(ctx.client_hub());

        info!("OAGW gear initialized");
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

        let service = self
            .service
            .get()
            .ok_or_else(|| anyhow::anyhow!("OAGW service not initialized"))?
            .clone();

        let router = crate::api::routes::register_routes(router, openapi, service);

        info!("OAGW REST routes registered successfully");
        Ok(router)
    }
}
