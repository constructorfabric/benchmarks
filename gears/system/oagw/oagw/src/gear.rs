//! Gear entry point of the OAGW outbound API gateway.
//!
//! [`OagwGear`] wires the gear configuration into the shared
//! [`RegistryStore`] that the control plane (slice 2) and the data plane
//! (slice 4) both read, and installs the ADR-0007 error-source middleware on
//! the REST surface.
//!
//! ## Lifecycle
//!
//! `init` is synchronous and fail-fast: an invalid configuration must abort
//! gear start-up rather than surface as a per-request failure. `serve` then
//! parks until the runtime cancels it — this gear owns no background workers
//! in slice 1 (no token refreshers, no reconcilers), and the proxy engine
//! arriving in slice 4 is driven by the REST surface rather than by a worker
//! loop.

use std::sync::Arc;

use async_trait::async_trait;
use credstore_sdk::CredStoreClientV1;
use tenant_resolver_sdk::TenantResolverClient;
use toolkit::api::OpenApiRegistry;
use toolkit::{Gear, GearCtx, RestApiCapability};

use crate::api::rest::error_source_layer;
use crate::api::rest::handlers::proxy::DataPlane;
use crate::config::OagwConfig;
use crate::domain::metrics::MetricsRegistry;
use crate::domain::rate_limit::RateLimiterRegistry;
use crate::domain::services::{
    ControlPlaneService, NoHierarchy, ResolverHierarchy, TenantHierarchy,
};
use crate::infra::plugin::{
    CredStoreSecretResolver, PluginRegistry, SecretResolverTrait, TokenCacheConfig,
    UnavailableSecretResolver,
};
use crate::infra::proxy::ProxyEngine;
use crate::infra::storage::{CacheLimits, RegistryStore};
use crate::{api::rest::routes as rest_routes, domain::validation::Validator};

/// Cache budgets derived from the gear configuration (ADR-0006: the control
/// plane owns the `upstream`/`route`/`plugin` L1 caches, the data plane the
/// `dp` one; both are provisioned from the same operator knobs).
fn cache_limits(config: &OagwConfig) -> CacheLimits {
    CacheLimits {
        upstream: config.upstream_l1_cache_max_entries,
        route: config.route_l1_cache_max_entries,
        plugin: config.plugin_l1_cache_max_entries,
        dp: config.dp_cache_max_entries,
    }
}

/// OAGW outbound API gateway gear.
///
/// Declares the dependencies the design assigns to this gear (`credstore` for
/// secrets, `tenant_resolver` for ancestor chains, `types_registry` for GTS
/// types and `authz_resolver` for tenant-scoped authorisation) and the `rest`
/// capability it exposes. The `stateful` capability is not declared: the
/// toolkit generates its lifecycle entry point from
/// `lifecycle(entry = "serve")` using `tokio_util::sync::CancellationToken`,
/// which is not a direct dependency of this crate (see `Cargo.toml`), and the
/// registry is process-local state exposed through REST rather than a
/// stateful capability contract.
#[toolkit::gear(
    name = "oagw",
    deps = [credstore, tenant_resolver, types_registry, authz_resolver],
    capabilities = [rest]
)]
pub struct OagwGear {
    /// Registry built during `init`; `None` before start-up completes.
    registry: std::sync::OnceLock<Arc<RegistryStore>>,
    /// Effective configuration captured during `init`.
    config: std::sync::OnceLock<OagwConfig>,
    /// Control-plane service built during `init`.
    service: std::sync::OnceLock<Arc<ControlPlaneService>>,
    /// Plugin registry built during `init` (ADR-0002).
    plugins: std::sync::OnceLock<Arc<PluginRegistry>>,
    /// Outbound proxy engine built during `init` (ADR-0006).
    engine: std::sync::OnceLock<Arc<ProxyEngine>>,
    /// Data-plane metrics registry built during `init`.
    metrics: std::sync::OnceLock<Arc<MetricsRegistry>>,
    /// Rate-limit buckets (ADR-0003), shared by the data plane and the store.
    ///
    /// The store keeps a handle so an upstream mutation can drop the buckets
    /// that belong to it; the data plane keeps the same handle so both sides
    /// see one set of budgets.
    rate_limiters: std::sync::OnceLock<Arc<RateLimiterRegistry>>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            registry: std::sync::OnceLock::new(),
            config: std::sync::OnceLock::new(),
            service: std::sync::OnceLock::new(),
            plugins: std::sync::OnceLock::new(),
            engine: std::sync::OnceLock::new(),
            metrics: std::sync::OnceLock::new(),
            rate_limiters: std::sync::OnceLock::new(),
        }
    }
}

impl OagwGear {
    /// Registry built during `init`, or `None` before start-up completes.
    #[must_use]
    pub fn registry(&self) -> Option<&Arc<RegistryStore>> {
        self.registry.get()
    }

    /// Configuration captured during `init`, or `None` before start-up.
    #[must_use]
    pub fn effective_config(&self) -> Option<&OagwConfig> {
        self.config.get()
    }

    /// Control-plane service built during `init`, or `None` before start-up.
    #[must_use]
    pub fn service(&self) -> Option<&Arc<ControlPlaneService>> {
        self.service.get()
    }

    /// Plugin registry built during `init`, or `None` before start-up.
    ///
    /// The registry is fail-closed: without a `credstore` dependency it still
    /// exposes every built-in, but the `cred://`-backed auth plugins fail at
    /// use time with `500 SecretNotFound` instead of proxying unauthenticated.
    #[must_use]
    pub fn plugins(&self) -> Option<&Arc<PluginRegistry>> {
        self.plugins.get()
    }

    /// Builds the ADR-0002 plugin registry over the best available credential
    /// resolver.
    fn build_plugins(
        config: &OagwConfig,
        resolver: Arc<dyn SecretResolverTrait>,
    ) -> PluginRegistry {
        PluginRegistry::with_builtins(resolver, TokenCacheConfig::from(config))
    }

    /// The control-plane service, or an error when `init` has not run.
    fn service_or(&self) -> anyhow::Result<Arc<ControlPlaneService>> {
        self.service
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("oagw control-plane service not initialized"))
    }

    /// The plugin registry, or an error when `init` has not run.
    fn plugins_or(&self) -> anyhow::Result<Arc<PluginRegistry>> {
        self.plugins
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("oagw plugin registry not initialized"))
    }

    /// The proxy engine, or an error when `init` has not run.
    fn engine_or(&self) -> anyhow::Result<Arc<ProxyEngine>> {
        self.engine
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("oagw proxy engine not initialized"))
    }

    /// The metrics registry, or an error when `init` has not run.
    fn metrics_or(&self) -> anyhow::Result<Arc<MetricsRegistry>> {
        self.metrics
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("oagw metrics registry not initialized"))
    }

    /// Builds the data-plane extension the proxy handlers read.
    fn build_data_plane(&self) -> anyhow::Result<DataPlane> {
        Ok(DataPlane {
            service: self.service_or()?,
            engine: self.engine_or()?,
            metrics: self.metrics_or()?,
            plugins: self.plugins_or()?,
            rate_limiters: self.rate_limiters(),
        })
    }

    /// The one rate-limit registry of this process, creating it on first use.
    ///
    /// `init` hands the same handle to the [`RegistryStore`], which is what
    /// makes an upstream mutation drop the buckets of the upstream it touched.
    fn rate_limiters(&self) -> Arc<RateLimiterRegistry> {
        Arc::clone(
            self.rate_limiters
                .get_or_init(|| Arc::new(RateLimiterRegistry::default())),
        )
    }

    /// Builds the control-plane service over a fresh registry.
    fn build_service(
        config: &OagwConfig,
        registry: Arc<RegistryStore>,
        hierarchy: Arc<dyn TenantHierarchy>,
    ) -> ControlPlaneService {
        ControlPlaneService::new(registry, Validator::new(config.clone()), hierarchy)
    }

    /// Service loop. Slice 1 owns no background workers, so the future parks
    /// until the runtime drops it.
    pub async fn serve(self: Arc<Self>) -> anyhow::Result<()> {
        std::future::pending::<()>().await;
        Ok(())
    }
}

#[async_trait]
impl Gear for OagwGear {
    /// Loads and validates the gear configuration and builds the registry.
    ///
    /// # Errors
    ///
    /// Returns the underlying error when the configuration cannot be decoded
    /// or fails [`OagwConfig::validate`].
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let config: OagwConfig = ctx.config_or_default()?;
        config.validate()?;
        let limits = cache_limits(&config);
        let registry = Arc::new(RegistryStore::new(limits));
        // The store learns the rate-limit registry before the data plane does,
        // so an upstream deleted through the management surface takes its
        // buckets with it from the very first request.
        registry.attach_rate_limiters(self.rate_limiters());
        let hierarchy: Arc<dyn TenantHierarchy> =
            match ctx.client_hub().try_get::<dyn TenantResolverClient>() {
                Some(client) => Arc::new(ResolverHierarchy::new(client)),
                None => Arc::new(NoHierarchy),
            };
        let service = Self::build_service(&config, Arc::clone(&registry), hierarchy);
        let resolver: Arc<dyn SecretResolverTrait> =
            match ctx.client_hub().try_get::<dyn CredStoreClientV1>() {
                Some(client) => Arc::new(CredStoreSecretResolver::new(client)),
                None => Arc::new(UnavailableSecretResolver),
            };
        let plugins = Self::build_plugins(&config, resolver);
        let metrics = Arc::new(MetricsRegistry::new());
        let engine = Arc::new(ProxyEngine::new(&config, Arc::clone(&metrics)));
        self.config
            .set(config)
            .map_err(|_| anyhow::anyhow!("oagw gear already initialised"))?;
        self.registry
            .set(registry)
            .map_err(|_| anyhow::anyhow!("oagw gear already initialised"))?;
        self.service
            .set(Arc::new(service))
            .map_err(|_| anyhow::anyhow!("oagw gear already initialised"))?;
        self.plugins
            .set(Arc::new(plugins))
            .map_err(|_| anyhow::anyhow!("oagw gear already initialised"))?;
        self.engine
            .set(engine)
            .map_err(|_| anyhow::anyhow!("oagw gear already initialised"))?;
        self.metrics
            .set(metrics)
            .map_err(|_| anyhow::anyhow!("oagw gear already initialised"))?;
        Ok(())
    }
}

impl RestApiCapability for OagwGear {
    /// Registers the fifteen management operations, the two proxy operations
    /// and the metrics endpoint, each family wrapped in its own ADR-0007
    /// error-source layer and `Extension`.
    ///
    /// The oagw routes are built on fresh sub-routers and then merged: layers
    /// only affect the routes registered before them, so a `/healthz` route
    /// the platform already mounted on the incoming router stays outside the
    /// oagw middleware.
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        let service = self.service_or()?;
        let management = rest_routes::register_routes(axum::Router::new(), openapi)
            .layer(axum::Extension(service))
            .layer(axum::middleware::from_fn(error_source_layer));
        let data_plane = self.build_data_plane()?;
        let data = rest_routes::register_proxy_routes(axum::Router::new(), openapi)
            .layer(axum::Extension(data_plane))
            .layer(axum::middleware::from_fn(error_source_layer));
        Ok(router.merge(management).merge(data))
    }
}

#[cfg(test)]
#[path = "gear_tests.rs"]
mod tests;
