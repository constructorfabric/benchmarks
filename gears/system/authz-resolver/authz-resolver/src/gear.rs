//! `AuthZ` resolver gear.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use authz_resolver_sdk::AuthZResolverClient;
use toolkit::Gear;
use toolkit::context::GearCtx;
use toolkit::contracts::SystemCapability;
use tracing::info;

use crate::config::AuthZResolverConfig;
use crate::domain::{AuthZResolverLocalClient, Service};

/// `AuthZ` Resolver gear.
///
/// This gear:
/// 1. Discovers plugin instances via types-registry
/// 2. Routes requests to the selected plugin based on vendor configuration
///
/// The `AuthZResolverPluginSpecV1` schema itself reaches `types-registry`
/// automatically via the `toolkit-gts` link-time inventory — no per-init
/// registration is needed. Plugin discovery is lazy: happens on first API
/// call after types-registry is ready.
#[toolkit::gear(
    name = "authz-resolver",
    deps = [types_registry],
    capabilities = [system]
)]
pub(crate) struct AuthZResolver {
    service: OnceLock<Arc<Service>>,
}

impl Default for AuthZResolver {
    fn default() -> Self {
        Self {
            service: OnceLock::new(),
        }
    }
}

// Marked as `system` so that init() runs in the system-gear phase.
// This ensures the AuthZResolver client is available in ClientHub before
// other system gears that depend on it.
impl SystemCapability for AuthZResolver {}

#[async_trait]
impl Gear for AuthZResolver {
    #[tracing::instrument(skip_all, fields(vendor))]
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: AuthZResolverConfig = ctx.config_or_default()?;
        tracing::Span::current().record("vendor", cfg.vendor.as_str());
        info!(vendor = %cfg.vendor);

        // Create service
        let hub = ctx.client_hub();
        let svc = Arc::new(Service::new(hub, cfg.vendor));
        self.service
            .set(svc.clone())
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        // Register client in ClientHub
        let api: Arc<dyn AuthZResolverClient> = Arc::new(AuthZResolverLocalClient::new(svc));
        ctx.client_hub().register::<dyn AuthZResolverClient>(api);

        Ok(())
    }
}
