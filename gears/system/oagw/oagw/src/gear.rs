//! Gear declaration for the OAGW gear.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use toolkit::api::OpenApiRegistry;
use toolkit::{Gear, GearCtx, RestApiCapability};
use tracing::info;

use crate::config::OagwConfig;
use crate::domain::services::management::ControlPlaneService;
use crate::infra::plugin::{
    AuthPluginRegistry, GuardPluginRegistry, ResolvingPluginValidator, TransformPluginRegistry,
};
use crate::infra::proxy::DataPlaneServiceImpl;
use crate::infra::storage::{ControlPlaneStore, SharedStore};

/// OAGW gear.
///
/// Serves the outbound API gateway surface under `/oagw/v1/...`:
///
/// - Management CRUD for tenant-scoped upstreams, routes and custom plugins;
/// - `{METHOD} /oagw/v1/proxy/{alias}[/{path}]` forwarding into the resolved
///   upstream through the auth / guard / transform / rate-limit / CORS
///   pipeline (see `docs/DESIGN.md`).
///
/// The control-plane store is in-memory (no persistence); the data-plane
/// service owns the outbound HTTP client, rate limiter and round-robin state.
/// The built-in auth plugins require the `credstore` gear's client from the
/// client hub at init (the token-cache creds flow, ADR-0008); the tenant
/// resolver is best-effort and degrades to a single-tenant chain when absent.
#[toolkit::gear(
    name = "oagw",
    capabilities = [rest]
)]
pub struct OagwGear {
    cp: OnceLock<Arc<ControlPlaneService>>,
    dp: OnceLock<Arc<DataPlaneServiceImpl>>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            cp: OnceLock::new(),
            dp: OnceLock::new(),
        }
    }
}

#[async_trait]
impl Gear for OagwGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: OagwConfig = ctx.config_or_default()?;
        info!(
            proxy_timeout_secs = cfg.proxy_timeout_secs,
            request_timeout_secs = cfg.request_timeout_secs,
            body_limit_bytes = cfg.body_limit_bytes,
            allow_http_upstream = cfg.allow_http_upstream,
            ssrf_policy_enabled = cfg.ssrf_policy.enabled,
            "Initializing OAGW gear"
        );

        // Credentials (ADR-0008 OAuth2 client-credentials auth, ADR-0009
        // API-key auth) resolve at call time through the shared credstore
        // client; references are stored and forwarded, never the secrets.
        let credstore: Arc<dyn credstore_sdk::CredStoreClientV1> = ctx
            .client_hub()
            .get::<dyn credstore_sdk::CredStoreClientV1>()
            .map_err(|e| anyhow::anyhow!("failed to get CredStoreClientV1: {e}"))?;

        // Tenant hierarchy resolution is best-effort: a missing resolver
        // yields a single-tenant chain (DESIGN "Tenant hierarchy").
        let tenant_resolver: Option<Arc<dyn tenant_resolver_sdk::TenantResolverClient>> = ctx
            .client_hub()
            .get::<dyn tenant_resolver_sdk::TenantResolverClient>()
            .ok();

        let store: SharedStore = Arc::new(ControlPlaneStore::new());

        // Built-in plugin registries (ADR-0008 wiring).
        let auth = AuthPluginRegistry::with_builtins(
            Arc::clone(&credstore),
            cfg.token_cache,
            toolkit_http::HttpClientConfig::proxy(),
        );
        let guards = GuardPluginRegistry::with_builtins();
        let transforms = TransformPluginRegistry::with_builtins();

        let validator = Arc::new(ResolvingPluginValidator::new(
            Arc::clone(&store),
            auth.clone(),
            guards.clone(),
            transforms.clone(),
        ));

        let cfg_arc = Arc::new(cfg.clone());
        let cp = Arc::new(ControlPlaneService::new(
            Arc::clone(&store),
            tenant_resolver,
            Arc::clone(&cfg_arc),
            validator,
        ));
        let dp = Arc::new(DataPlaneServiceImpl::new(
            Arc::clone(&cp),
            cfg_arc,
            auth,
            guards,
            transforms,
        ));

        self.cp
            .set(cp.clone())
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
        self.dp
            .set(dp.clone())
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

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

        let cp = self
            .cp
            .get()
            .ok_or_else(|| anyhow::anyhow!("Control plane service not initialized"))?
            .clone();
        let dp = self
            .dp
            .get()
            .ok_or_else(|| anyhow::anyhow!("Data plane service not initialized"))?
            .clone();

        let router = crate::api::rest::routes::register_routes(router, openapi, cp, dp);

        info!("OAGW REST routes registered successfully");
        Ok(router)
    }
}
