//! Gear declaration for the OAGW (Outbound API Gateway) gear.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::SystemCapability;
use toolkit::{Gear, GearCtx, RestApiCapability};
use tracing::info;

use crate::config::OagwConfig;
use crate::domain::service::ControlPlaneService;
use crate::infra::proxy::HttpProxyEngine;

/// OAGW gear.
///
/// ## Capabilities
///
/// - `system` — control-plane state is process-local; no `db` capability is
///   declared because the gear has no `database:` block (DESIGN §3.2) and
///   persists its management state in memory.
/// - `rest` — exposes the 15 management routes and the data plane under
///   `/api/oagw/v1`.
///
/// ## Data plane
///
/// [`HttpProxyEngine`] owns the proxy pipeline (DESIGN §3.5): route and
/// upstream resolution, plugin chain, target-host and body guards, header
/// transformation, rate limiting and metrics. One instance is built per
/// process and shared by the `rest` capability.
#[toolkit::gear(
    name = "oagw",
    deps = [types_registry, credstore],
    capabilities = [system, rest]
)]
pub struct OagwGear {
    service: OnceLock<Arc<ControlPlaneService>>,
    engine: OnceLock<Arc<HttpProxyEngine>>,
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
    /// Builds a service from the gear configuration.
    ///
    /// Exposed as a seam so the REST layer can be constructed and exercised
    /// without a running [`GearCtx`].
    #[must_use]
    pub fn build_service(config: &OagwConfig) -> Arc<ControlPlaneService> {
        let store = crate::infra::storage::InMemoryStore::new();
        Arc::new(ControlPlaneService::new(store, config))
    }

    /// Builds the data-plane engine over `service`.
    ///
    /// Review evidence (privilege boundary — credential resolution):
    /// * Guardrail: ADR 0008 "Secrets" + DESIGN §4.3 — plugin credentials are
    ///   resolved through the [`crate::infra::plugin::SecretResolver`] seam and
    ///   never logged or echoed.
    /// * Rationale: the engine is the only component that holds the resolver,
    ///   so a secret can only enter the pipeline through the audited
    ///   auth-plugin path.
    /// * Validation performed: `plugin_tests` asserts a `cred://` reference
    ///   that cannot be resolved fails closed with `SecretNotFound`.
    ///
    /// # Errors
    ///
    /// Returns the [`crate::infra::transport::Transport`] failure when the
    /// platform trust store cannot be loaded.
    pub fn build_engine(
        config: &OagwConfig,
        service: &Arc<ControlPlaneService>,
    ) -> Result<Arc<HttpProxyEngine>, crate::domain::error::DomainError> {
        let transport = Arc::new(crate::infra::transport::Transport::new(
            config.proxy_timeout(),
        )?);
        let secrets: std::sync::Arc<dyn crate::infra::plugin::SecretResolver> =
            std::sync::Arc::new(crate::infra::plugin::LiteralSecretResolver);
        let registry = crate::infra::plugin::registry(&crate::infra::plugin::PluginBundle {
            secrets,
            transport: Arc::clone(&transport),
            token_cache_ttl: config.token_cache_ttl(),
            token_cache_capacity: config.token_cache_capacity,
        });
        Ok(Arc::new(HttpProxyEngine::new(
            Arc::clone(service),
            crate::infra::tenant::TenantChainResolver::new(None),
            transport,
            registry,
            crate::infra::ratelimit::LimiterRegistry::new(),
            std::sync::Arc::new(crate::infra::metrics::MetricsRegistry::new()),
            config.clone(),
        )))
    }
}

#[async_trait]
impl Gear for OagwGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: OagwConfig = ctx.config_or_default()?;
        info!(
            proxy_timeout_secs = cfg.proxy_timeout_secs,
            allow_http_upstream = cfg.allow_http_upstream,
            token_cache_ttl_secs = cfg.token_cache_ttl_secs,
            token_cache_capacity = cfg.token_cache_capacity,
            max_payload_bytes = cfg.max_payload_bytes,
            ssrf_policy_enabled = cfg.ssrf_policy.enabled,
            "Loaded oagw config"
        );

        let service = Self::build_service(&cfg);
        let engine = Self::build_engine(&cfg, &service)?;
        self.service
            .set(service)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
        self.engine
            .set(engine)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
        Ok(())
    }
}

#[async_trait]
impl SystemCapability for OagwGear {}

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
            .ok_or_else(|| anyhow::anyhow!("Service not initialized"))?
            .clone();
        let engine = self
            .engine
            .get()
            .ok_or_else(|| anyhow::anyhow!("Data-plane engine not initialized"))?
            .clone();
        let router = crate::api::rest::routes::register_routes(router, openapi, service);
        let router = crate::api::rest::routes::register_data_plane(router, engine);
        info!("OAGW REST routes registered successfully");
        Ok(router)
    }
}
