//! Gear wiring: configuration, repositories, service and REST registration.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::RestApiCapability;
use toolkit::{Gear, GearCtx, gear};
use tracing::info;

use crate::config::OagwConfig;
use crate::domain::repo::AllowAllAuthorizer;
use crate::domain::services::ControlPlaneService;
use crate::infra::plugin::registry::AuthPluginRegistry;
use crate::infra::plugin::secrets::HubSecretResolver;
use crate::infra::proxy::forward::ProxyEngine;
use crate::infra::proxy::policy::ProxyPolicy;
use crate::infra::proxy::resolver::StaticHierarchy;
use crate::infra::proxy::ssrf::SsrfGuard;
use crate::infra::proxy::transport::UpstreamTransport;
use crate::infra::storage::{
    MemoryPluginRepository, MemoryRouteRepository, MemoryStore, MemoryUpstreamRepository,
};

/// The OAGW gear: control plane for outbound upstreams, routes and plugins.
#[gear(name = "oagw", deps = [], capabilities = [rest])]
pub struct OagwGear {
    service: OnceLock<Arc<ControlPlaneService>>,
    engine: OnceLock<Arc<ProxyEngine>>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            service: OnceLock::new(),
            engine: OnceLock::new(),
        }
    }
}

impl OagwGear {
    /// The service this gear assembled at init.
    #[must_use]
    pub fn service(&self) -> Option<Arc<ControlPlaneService>> {
        self.service.get().cloned()
    }

    /// The data plane this gear assembled at init.
    #[must_use]
    pub fn engine(&self) -> Option<Arc<ProxyEngine>> {
        self.engine.get().cloned()
    }
}

#[async_trait]
impl Gear for OagwGear {
    #[tracing::instrument(skip_all, fields(module = "oagw"))]
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let config: OagwConfig = ctx.config_or_default()?;
        config
            .validate()
            .map_err(|error| anyhow::anyhow!("oagw config invalid: {error}"))?;
        info!(
            proxy_timeout_secs = config.proxy_timeout_secs,
            allow_http_upstream = config.allow_http_upstream,
            ssrf_policy = config.ssrf_policy.enabled,
            "initializing oagw module"
        );

        let store = Arc::new(MemoryStore::new());
        let upstreams = Arc::new(MemoryUpstreamRepository::new(Arc::clone(&store)));
        let routes = Arc::new(MemoryRouteRepository::new(Arc::clone(&store)));
        let plugins = Arc::new(MemoryPluginRepository::new(
            Arc::clone(&store),
            Arc::clone(&upstreams) as Arc<dyn crate::domain::repo::UpstreamRepository>,
            Arc::clone(&routes) as Arc<dyn crate::domain::repo::RouteRepository>,
        ));
        // No PDP projection exists for this gear yet; the control plane is
        // authorized by the authenticated router and admits every identified
        // caller. A policy-backed `ManagementAuthorizer` replaces this without
        // touching the services.
        let authorizer = Arc::new(AllowAllAuthorizer);

        let service = Arc::new(
            ControlPlaneService::new(
                Arc::clone(&upstreams) as Arc<dyn crate::domain::repo::UpstreamRepository>,
                Arc::clone(&routes) as Arc<dyn crate::domain::repo::RouteRepository>,
                plugins,
                authorizer,
                config.list_default_page_size,
                config.list_max_page_size,
            )
            // One posture, two gates: the management API admits what the
            // deployment opted into and the data plane enforces it again, so the
            // `allow_http_upstream` opt-in cannot be widened by either side alone.
            .with_cleartext_endpoints(config.allow_http_upstream),
        );

        // The data plane shares the control plane's repositories, so an alias
        // created over the management API is routable at once (`DESIGN` §3.2).
        // The tenant chain is the caller's own tenant: this gear has no tenant
        // resolver dependency, so hierarchical shadowing of an ancestor alias is
        // not resolved until that dependency is declared.
        let policy = ProxyPolicy::new(config.proxy_timeout_secs, config.allow_http_upstream);
        let transport = UpstreamTransport::new(&policy)
            .map_err(|error| anyhow::anyhow!("oagw transport unavailable: {error}"))?;
        let engine = Arc::new(ProxyEngine::new(
            upstreams as Arc<dyn crate::domain::repo::UpstreamRepository>,
            routes as Arc<dyn crate::domain::repo::RouteRepository>,
            Arc::new(StaticHierarchy::new(Vec::new())),
            AuthPluginRegistry::with_builtins(
                Arc::new(HubSecretResolver::new(ctx.client_hub())),
                None,
                crate::infra::plugin::oauth2_client_cred_auth::TokenCacheConfig::default(),
            ),
            SsrfGuard::new(config.ssrf_policy.enabled),
            policy,
            transport,
        ));

        self.service
            .set(service)
            .map_err(|_| anyhow::anyhow!("{} module already initialized", Self::MODULE_NAME))?;
        self.engine
            .set(engine)
            .map_err(|_| anyhow::anyhow!("{} module already initialized", Self::MODULE_NAME))?;

        info!("oagw module initialized");
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
        let service = self
            .service
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("oagw ControlPlaneService not initialized"))?;
        let engine = self
            .engine
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("oagw ProxyEngine not initialized"))?;
        let router = crate::api::rest::register_routes(router, openapi, service, engine);
        info!("oagw REST routes registered");
        Ok(router)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "gear_tests.rs"]
mod tests;
