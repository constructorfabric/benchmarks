//! OAGW gear definition: bootstraps the control plane, the data plane and the
//! REST surface.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use toolkit::api::OpenApiRegistry;
use toolkit::{Gear, GearCtx, RestApiCapability};

use crate::api::handlers::Services;
use crate::api::routes;
use crate::config::OagwConfig;
use crate::domain::service::{ControlPlaneService, TenantHierarchy};
use crate::infra::proxy::service::DataPlaneService;
use crate::infra::secrets::SecretResolver;
use crate::infra::storage::MemoryStore;

/// Outbound API gateway gear.
#[toolkit::gear(name = "oagw", capabilities = [rest])]
pub struct OagwGear {
    /// Shared handler services, built once during `init`.
    services: OnceLock<Arc<Services>>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            services: OnceLock::new(),
        }
    }
}

impl OagwGear {
    /// Access the services after `init`.
    #[must_use]
    pub fn services(&self) -> Option<Arc<Services>> {
        self.services.get().cloned()
    }
}

#[async_trait]
impl Gear for OagwGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let config = ctx.config_or_default::<crate::config::OagwConfig>()?;

        // Control plane owns the in-process configuration store.
        let upstreams = Arc::new(MemoryStore::default());
        let route_store = Arc::new(MemoryStore::default());
        let plugin_store = Arc::new(MemoryStore::default());
        let control = Arc::new(ControlPlaneService::new(
            upstreams,
            route_store,
            plugin_store,
            config.allow_http_upstream,
        ));

        // Data plane shares the same stores so a configured upstream is
        // immediately proxyable.
        let registry = Arc::new(crate::infra::plugins::PluginRegistry::builtin());
        let secrets = SecretResolver::from_ctx(ctx);
        let data = Arc::new(DataPlaneService::new(
            control.clone(),
            hierarchy_from(ctx),
            registry,
            secrets,
            tls_settings(&config),
            config.clone(),
        ));

        let services = Arc::new(Services {
            control,
            data,
            config,
        });
        self.services
            .set(services)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        tracing::info!("OAGW gear initialized");
        Ok(())
    }
}

/// Build the tenant hierarchy source used to resolve inherited configuration.
fn hierarchy_from(ctx: &GearCtx) -> Arc<dyn TenantHierarchy> {
    if let Ok(resolver) = ctx.client_hub().get::<dyn tenant_resolver_sdk::TenantResolverClient>() {
        Arc::new(crate::domain::service::ResolverHierarchy::new(resolver))
    } else {
        Arc::new(crate::domain::service::FlatHierarchy)
    }
}

#[async_trait]
impl RestApiCapability for OagwGear {
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
            .ok_or_else(|| anyhow::anyhow!("OAGW services not initialized"))?;
        tracing::info!("Registering OAGW REST routes");
        Ok(routes::register_routes(router, openapi, (*services).clone()))
    }
}

/// Data-plane TLS settings built from the gear configuration.
fn tls_settings(config: &OagwConfig) -> Option<crate::infra::proxy::transport::TlsSettings> {
    let tls = crate::infra::proxy::transport::TlsSettings::from_native_roots(
        config.allow_insecure_tls,
    )
    .map_err(|err| tracing::warn!(error = %err, "TLS transport unavailable"))
    .ok();
    if tls.is_none() {
        tracing::warn!("HTTPS upstreams will fail: no TLS transport configured");
    }
    tls
}
