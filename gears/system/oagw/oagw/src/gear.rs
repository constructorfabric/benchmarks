//! Gear declaration for the OAGW (Outbound API Gateway) gear.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::SystemCapability;
use toolkit::runtime::SystemContext;
use toolkit::{Gear, GearCtx, RestApiCapability};
use tracing::{debug, info};

use crate::config::OagwConfig;
use crate::domain::store::ConfigService;
use crate::proxy::ProxyService;

/// OAGW gear.
///
/// Centralized outbound API gateway that manages all outbound API requests
/// from gears to external services: routing, authentication, rate limiting and
/// monitoring through a unified proxy layer. Configuration lives in the
/// control plane (upstreams, routes, plugins) while the data plane forwards
/// requests; both are implemented as domain traits inside this crate
/// (DESIGN.md §1.1, §1.3).
///
/// ## Capabilities
///
/// - `rest` — exposes the management REST API and the proxy data plane
/// - `system` — core infrastructure gear, initialized early in startup
///
/// ## Error contract
///
/// Every response this gear produces carries `X-OAGW-Error-Source`
/// (see [`crate::error`]).
#[toolkit::gear(
    name = "oagw",
    capabilities = [rest, system]
)]
pub struct OagwGear {
    config: OnceLock<OagwConfig>,
    services: OnceLock<GatewayServices>,
}

/// The runtime services of one gateway: the control plane's configuration
/// store and the data plane that serves `/oagw/v1/proxy/{alias}/…`.
///
/// Both are built once, from the same [`OagwConfig`], and shared by every
/// request the gear serves.
struct GatewayServices {
    /// Upstream, route and plugin configuration (DESIGN.md §3.1).
    config: Arc<ConfigService>,
    /// The proxy data plane.
    proxy: Arc<ProxyService>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            config: OnceLock::new(),
            services: OnceLock::new(),
        }
    }
}

impl OagwGear {
    /// The configuration loaded during [`Gear::init`], or `None` before the
    /// gear has been initialized.
    #[must_use]
    pub fn config(&self) -> Option<&OagwConfig> {
        self.config.get()
    }

    /// The gateway services, built from the loaded configuration the first
    /// time they are needed.
    ///
    /// Registration can run before or after [`Gear::init`], so the services are
    /// built lazily from whatever configuration is available rather than
    /// assumed to exist.
    fn services(&self) -> anyhow::Result<&GatewayServices> {
        if let Some(services) = self.services.get() {
            return Ok(services);
        }

        let config = self.config.get().cloned().unwrap_or_default();
        let config_service = Arc::new(ConfigService::new(config));
        let proxy = ProxyService::new(config_service.clone())
            .map_err(|error| anyhow::anyhow!("{error}"))?;

        // A concurrent registration would have built the same services from the
        // same configuration, so whichever wins is equivalent.
        let services = GatewayServices {
            config: config_service,
            proxy: Arc::new(proxy),
        };
        let _unused = self.services.set(services);

        self.services
            .get()
            .ok_or_else(|| anyhow::anyhow!("oagw services unavailable"))
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
            "Loaded oagw config"
        );

        self.config
            .set(cfg)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        info!("oagw gear initialized");

        Ok(())
    }
}

#[async_trait]
impl SystemCapability for OagwGear {
    /// Post-init hook: logs that the gateway is wired up and ready to serve.
    ///
    /// Runs AFTER `init()` has completed for all gears.
    async fn post_init(&self, _sys: &SystemContext) -> anyhow::Result<()> {
        info!(
            config_loaded = self.config().is_some(),
            "oagw gear post_init complete"
        );

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
        let services = self.services()?;
        info!(
            config_loaded = self.config().is_some(),
            "Registering oagw REST routes"
        );

        // The management REST API (control plane) and the proxy data plane
        // share one router and one configuration service: `/oagw/v1/upstreams`,
        // `/oagw/v1/routes`, … and `/oagw/v1/proxy/{alias}/…` never overlap.
        let management = Arc::new(crate::api::ManagementService::new(services.config.clone()));

        Ok(crate::proxy::register_proxy_routes(
            crate::api::rest::register_routes(router, openapi, management),
            services.proxy.clone(),
        ))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn test_default_gear_has_no_config() {
        let gear = OagwGear::default();

        assert!(gear.config().is_none());
        assert!(gear.config.get().is_none());
    }

    #[test]
    fn test_module_name_is_the_gear_name() {
        assert_eq!(OagwGear::MODULE_NAME, "oagw");
    }

    #[test]
    fn test_config_can_be_published_once() {
        let gear = OagwGear::default();

        assert!(gear.config.set(OagwConfig::default()).is_ok());
        assert!(gear.config.set(OagwConfig::default()).is_err());

        let stored = gear.config().unwrap();
        assert_eq!(stored.proxy_timeout_secs, 30);
    }
}
