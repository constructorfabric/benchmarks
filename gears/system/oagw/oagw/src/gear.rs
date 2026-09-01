//! Gear declaration for the OAGW (outbound API gateway) gear.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use credstore_sdk::CredStoreClientV1;
use tenant_resolver_sdk::TenantResolverClient;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::SystemCapability;
use toolkit::{Gear, GearCtx, RestApiCapability};
use tracing::{debug, info};

use crate::config::OagwConfig;
use crate::domain::service::ControlPlaneService;
use crate::infra::plugin::{AuthPluginRegistry, TokenCacheConfig};
use crate::infra::ratelimit::RateLimiter;

/// OAGW gear.
///
/// ## Capabilities
///
/// - `system` — core infrastructure gear
/// - `rest` — exposes REST (management + proxy) endpoints
///
/// ## Wiring
///
/// At `init`, reads `gears.oagw.config` (or defaults), builds the in-memory
/// control-plane service, and resolves the typed dependency clients
/// (credstore, types-registry, tenant-resolver, authz-resolver) from the
/// `ClientHub` for use by the data plane.
#[toolkit::gear(
    name = "oagw",
    capabilities = [system, rest],
    deps = [authz_resolver, tenant_resolver, types_registry, credstore]
)]
pub struct OagwGear {
    config: OnceLock<Arc<OagwConfig>>,
    service: OnceLock<Arc<ControlPlaneService>>,
    auth: OnceLock<Arc<AuthPluginRegistry>>,
    rate: OnceLock<Arc<RateLimiter>>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            config: OnceLock::new(),
            service: OnceLock::new(),
            auth: OnceLock::new(),
            rate: OnceLock::new(),
        }
    }
}

impl OagwGear {
    /// The frozen gear configuration, if `init` already ran.
    #[must_use]
    pub fn config(&self) -> Option<Arc<OagwConfig>> {
        self.config.get().cloned()
    }

    /// The control-plane service, if `init` already ran.
    #[must_use]
    pub fn service(&self) -> Option<Arc<ControlPlaneService>> {
        self.service.get().cloned()
    }

    /// The built-in auth-plugin registry, if `init` already ran.
    #[must_use]
    pub fn auth_plugins(&self) -> Option<Arc<AuthPluginRegistry>> {
        self.auth.get().cloned()
    }

    /// The DP-owned rate limiter, if `init` already ran (ADR 0006).
    #[must_use]
    pub fn rate_limiter(&self) -> Option<Arc<RateLimiter>> {
        self.rate.get().cloned()
    }
}

#[async_trait::async_trait]
impl Gear for OagwGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: OagwConfig = ctx.config_or_default()?;
        debug!(
            proxy_timeout_secs = cfg.proxy_timeout_secs,
            allow_http_upstream = cfg.allow_http_upstream,
            ssrf_enabled = cfg.ssrf_policy.enabled,
            token_cache_ttl_secs = cfg.token_cache_ttl_secs,
            token_cache_capacity = cfg.token_cache_capacity,
            "Loaded oagw config"
        );

        let service = ControlPlaneService::shared(cfg.clone());
        self.service
            .set(service)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
        self.config
            .set(Arc::new(cfg.clone()))
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        // Auth-plugin registry: resolves the `cred_store` client from the
        // `ClientHub` (absent → builtins that need secrets are left out; see
        // `AuthPluginRegistry::new`). The token-cache config comes from the
        // gear config (ADR 0008).
        let credstore = ctx.client_hub().get::<dyn CredStoreClientV1>().ok();
        let registry = AuthPluginRegistry::new(
            credstore,
            TokenCacheConfig {
                ttl: Duration::from_secs(cfg.token_cache_ttl_secs),
                capacity: cfg.token_cache_capacity,
            },
        );
        self.auth
            .set(Arc::new(registry))
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        // DP-owned per-instance rate limiter (ADR 0006): shared by every
        // proxy handler through the router's Extension layer.
        self.rate
            .set(Arc::new(RateLimiter::new()))
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        // Inter-gear clients (credstore, tenant-resolver, authz-resolver,
        // types-registry) are resolved from the `ClientHub` on demand by the
        // data plane (per-request), never cached at boot — provider gears may
        // register later or be absent from the binary entirely. The `deps`
        // list above guarantees link-time presence of the provider crates.

        info!("oagw gear initialized");
        Ok(())
    }
}

#[async_trait::async_trait]
impl SystemCapability for OagwGear {
    /// Post-init hook: nothing to publish for the MVP (types-registry
    /// provisioning of OAGW schemas is deferred; see DESIGN type provisioning).
    async fn post_init(&self, _sys: &toolkit::runtime::SystemContext) -> anyhow::Result<()> {
        debug!("oagw post_init complete");
        Ok(())
    }
}

impl RestApiCapability for OagwGear {
    fn register_rest(
        &self,
        ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        let service = self
            .service
            .get()
            .ok_or_else(|| anyhow::anyhow!("oagw service not initialized"))?
            .clone();

        // Optional tenant-resolver client: resolved at route registration when
        // the provider gear is present, else `None` (proxy falls back to the
        // caller's own tenant). Not boot-blocking — `deps` guarantees link-time
        // presence, initialization order guarantees availability in real runs.
        let tenant_resolver = ctx.client_hub().get::<dyn TenantResolverClient>().ok();

        let router = crate::api::rest::routes::register_routes(
            router,
            openapi,
            service,
            tenant_resolver,
            self.auth.get().cloned(),
            self.rate.get().cloned(),
        );
        info!("oagw REST routes registered");
        Ok(router)
    }
}
