//! Gear declaration: wires the control plane, the data plane and the REST API
//! into the host runtime.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use toolkit::Gear;
use toolkit::GearCtx;
use toolkit::RestApiCapability;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::SystemCapability;
use tracing::info;

use crate::config::OagwConfig;
use crate::domain::services::control_plane::ControlPlaneService;
use crate::infra::authz::Pep;
use crate::infra::plugin::registry::PluginRegistry;
use crate::infra::plugin::{ClientCredentialsAuth, Variant};
use crate::infra::proxy::service::OagwDataPlane;
use types_registry_sdk::TypesRegistryClient;
use crate::infra::storage::Storage;
use crate::api::rest::routes;
use crate::api::rest::state::OagwState;

/// The `oagw` gear — the outbound API gateway.
///
/// Stores upstreams, routes and plugins for the caller's tenant and forwards
/// proxied traffic to the winning endpoint.
#[toolkit::gear(
    name = "oagw",
    deps = [authz_resolver, tenant_resolver, credstore, types_registry],
    capabilities = [system, rest]
)]
pub struct OagwGear {
    state: OnceLock<Arc<OagwState>>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            state: OnceLock::new(),
        }
    }
}

impl OagwGear {
    /// The shared handler state, once the gear is initialised.
    pub fn state(&self) -> Option<Arc<OagwState>> {
        self.state.get().cloned()
    }
}

#[async_trait]
impl Gear for OagwGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let config: OagwConfig = ctx.config_or_default()?;
        info!(
            proxy_timeout_secs = config.proxy_timeout_secs,
            allow_http_upstream = config.allow_http_upstream,
            ssrf_enabled = config.ssrf_policy.enabled,
            "Loaded oagw config"
        );
        let config = Arc::new(config);

        let storage = Storage::new();
        let rate_limits = storage.rate_limits.clone();

        // The PEP is optional: without an authz resolver the gear falls back to
        // an allow-all scope, which is the single-tenant posture of a gear
        // running without the platform's policy decision point.
        let pep = ctx
            .client_hub()
            .try_get::<dyn authz_resolver_sdk::AuthZResolverClient>()
            .map(Pep::new);

        let types_registry: Option<Arc<dyn TypesRegistryClient>> =
            ctx.client_hub().try_get::<dyn TypesRegistryClient>();

        let plugins = Arc::new(plugins(config.as_ref(), &ctx));
        let control_plane = Arc::new(ControlPlaneService::new(
            storage.upstreams.clone(),
            storage.routes.clone(),
            storage.plugins.clone(),
            pep,
            Some(plugins.clone()),
        ));

        let data_plane = Arc::new(OagwDataPlane::new(
            control_plane.clone(),
            rate_limits,
            plugins,
            config.clone(),
        ));

        // The tenant resolver walks the caller's chain; without one the gear
        // degrades to a single-tenant chain rooted at the caller.
        let tenant_resolver: Option<Arc<dyn tenant_resolver_sdk::TenantResolverClient>> =
            ctx.client_hub().try_get::<dyn tenant_resolver_sdk::TenantResolverClient>();

        let state = Arc::new(OagwState {
            control_plane,
            data_plane,
            config,
            types_registry,
            tenant_resolver,
        });

        self.state
            .set(state)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        info!("oagw gear initialised");
        Ok(())
    }
}

/// Builds the plugin registry with the built-ins and the credstore-backed
/// OAuth2 client-credentials variants (ADR 0008).
fn plugins(config: &OagwConfig, ctx: &GearCtx) -> PluginRegistry {
    let mut registry = PluginRegistry::builtin();
    let credstore: Option<Arc<dyn credstore_sdk::CredStoreClientV1>> =
        ctx.client_hub().try_get::<dyn credstore_sdk::CredStoreClientV1>();
    if credstore.is_none() {
        tracing::warn!("credstore client unavailable: OAuth2 client credentials will fail open");
    }
    for variant in [Variant::Form, Variant::Basic] {
        registry.register_auth(Arc::new(ClientCredentialsAuth::new(
            variant,
            credstore.clone(),
            config.token_cache.ttl_secs,
            config.token_cache.capacity,
        )));
    }
    registry
}

#[async_trait]
impl SystemCapability for OagwGear {
    async fn post_init(&self, _sys: &toolkit::runtime::SystemContext) -> anyhow::Result<()> {
        // The catalog is descriptive; a build without types-registry simply
        // publishes nothing.
        let registry = self.state.get().and_then(|state| state.types_registry.clone());
        crate::infra::type_provisioning::provision(registry).await?;
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
        let state = self
            .state
            .get()
            .ok_or_else(|| anyhow::anyhow!("oagw: register_rest invoked before init"))?
            .clone();
        let router = routes::router(router, openapi).layer(axum::Extension(state));
        info!("oagw REST routes registered");
        Ok(router)
    }
}
