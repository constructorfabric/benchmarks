//! OAGW gear wiring: registration with the toolkit runtime.
//!
//! The control plane is in-memory, so the gear declares **no database
//! capability** and never touches `GearCtx::db` — the data plane and the
//! management CRUD are served from process state only.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use toolkit::api::OpenApiRegistry;
use toolkit::{Gear, GearCtx, RestApiCapability};
use tracing::info;

use crate::config::OagwConfig;
use crate::domain::control_plane::ControlPlane;

/// The OAGW gear: an outbound API gateway with a management (control plane)
/// REST surface and a proxy (data plane) surface.
#[toolkit::gear(
    name = "oagw",
    capabilities = [rest],
    deps = [types_registry]
)]
pub struct OagwGear {
    control_plane: OnceLock<Arc<ControlPlane>>,
    config: OnceLock<OagwConfig>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            control_plane: OnceLock::new(),
            config: OnceLock::new(),
        }
    }
}

impl OagwGear {
    /// The control plane backing the management REST surface, set by `init`.
    ///
    /// # Errors
    ///
    /// Returns an error when the gear was not initialized.
    fn control_plane(&self) -> anyhow::Result<Arc<ControlPlane>> {
        self.control_plane
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("oagw: register_rest invoked before init"))
    }

    /// The gear configuration, set by `init`.
    ///
    /// # Errors
    ///
    /// Returns an error when the gear was not initialized.
    fn config(&self) -> anyhow::Result<OagwConfig> {
        self.config
            .get()
            .copied()
            .ok_or_else(|| anyhow::anyhow!("oagw: register_rest invoked before init"))
    }
}

#[async_trait]
impl Gear for OagwGear {
    #[tracing::instrument(skip_all, fields(module = "oagw"))]
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        // Lenient: a deployment without an `oagw` section gets the documented
        // defaults, and unknown keys fail loudly here.
        let config: OagwConfig = ctx.config_or_default()?;
        info!(
            proxy_timeout_secs = config.proxy_timeout_secs,
            allow_http_upstream = config.allow_http_upstream,
            ssrf_policy_enabled = config.ssrf_policy.enabled,
            body_limit_bytes = config.body_limit_bytes,
            "initializing oagw module"
        );

        self.config
            .set(config)
            .map_err(|_| anyhow::anyhow!("oagw module already initialized"))?;
        self.control_plane
            .set(Arc::new(ControlPlane::new()))
            .map_err(|_| anyhow::anyhow!("oagw module already initialized"))?;

        info!("oagw module initialized (in-memory control plane)");
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
        info!("registering oagw REST routes");
        let control_plane = self.control_plane()?;
        let config = self.config()?;
        let router = crate::api::rest::register_routes(router, openapi, control_plane, config);
        info!("oagw REST routes registered");
        Ok(router)
    }
}
