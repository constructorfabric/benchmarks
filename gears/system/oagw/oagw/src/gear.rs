// Created: 2026-08-29 by Constructor Tech
//! Gear registration: the `oagw` outbound API gateway.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use credstore_sdk::CredStoreClientV1;
use tenant_resolver_sdk::TenantResolverClient;
use toolkit::Gear;
use toolkit::RestApiCapability;
use toolkit::api::OpenApiRegistry;
use toolkit::context::GearCtx;
use tracing::info;

use crate::config::OagwConfig;
use crate::domain::services::management::ControlPlaneService;
use crate::infra::plugin::PluginRegistry;
use crate::infra::proxy::service::DataPlaneService;
use crate::infra::storage::Stores;

/// The `oagw` outbound API gateway gear.
#[toolkit::gear(
    name = "oagw",
    deps = [credstore, types_registry, authz_resolver, tenant_resolver],
    capabilities = [rest]
)]
pub struct Oagw {
    services: OnceLock<Arc<crate::api::rest::handlers::Services>>,
}

impl Default for Oagw {
    fn default() -> Self {
        Self {
            services: OnceLock::new(),
        }
    }
}

impl Oagw {
    /// The services bundle built at init (available after `Gear::init`).
    #[must_use]
    pub fn services(&self) -> Option<Arc<crate::api::rest::handlers::Services>> {
        self.services.get().cloned()
    }
}

#[async_trait]
impl Gear for Oagw {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let config: OagwConfig = ctx.config_or_default()?;

        let credstore = ctx
            .client_hub()
            .get::<dyn CredStoreClientV1>()
            .map_err(|error| anyhow::anyhow!("failed to get CredStoreClientV1: {error}"))
            .ok();
        let tenant_resolver = ctx
            .client_hub()
            .get::<dyn TenantResolverClient>()
            .map_err(|error| anyhow::anyhow!("failed to get TenantResolverClient: {error}"))
            .ok();

        let stores = Arc::new(Stores::new());
        let control_plane = Arc::new(ControlPlaneService::new(
            stores.upstreams(),
            stores.routes(),
            stores.plugins(),
        ));
        let plugins = Arc::new(PluginRegistry::with_builtins(
            credstore,
            std::time::Duration::from_secs(config.token_cache_ttl_secs),
            config.token_cache_capacity,
        ));
        // Configuration can now reject a dangling plugin reference instead of
        // leaving it to fail with `503` on the first request.
        control_plane.set_plugin_catalog({
            let plugins = Arc::clone(&plugins);
            Arc::new(move |reference: &str| !plugins.missing(reference))
        });
        let data_plane = Arc::new(
            DataPlaneService::new(
                Arc::clone(&control_plane),
                Arc::clone(&plugins),
                tenant_resolver,
                config,
            )
            .map_err(|error| anyhow::anyhow!("oagw data plane: {error}"))?,
        );

        let services = Arc::new(crate::api::rest::handlers::Services {
            control_plane,
            data_plane,
        });
        self.services
            .set(services)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        info!("oagw gear initialized");
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
        let services = self
            .services
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("oagw services not initialized"))?;
        let router = crate::api::rest::routes::register(router, openapi, services);
        info!("oagw REST routes registered");
        Ok(router)
    }
}
