// Created: 2026-08-29 by Constructor Tech
//! ToolKit gear wiring (DESIGN §3.2 component model).
//!
//! The gear owns two planes: the control plane (`api::rest::handlers` over the
//! management service) and the data plane (`infra::proxy::ProxyEngine` behind
//! the [`ProxyStack`]). Everything is built once in [`Gear::init`] and
//! published to the REST layer through [`RestApiCapability::register_rest`].

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use toolkit::api::OpenApiRegistry;
use toolkit::{Gear, GearCtx, RestApiCapability};
use tracing::info;

use crate::api::rest::proxy::{PROXY_PREFIX, ProxyStack, proxy_handler};
use crate::api::rest::routes;
use crate::config::OagwConfig;
use crate::domain::services::management::ManagementService;
use crate::domain::services::proxy::ProxyService;
use crate::infra::hierarchy::FlatTenantHierarchy;
use crate::infra::http::OutboundClient;
use crate::infra::metrics::OagwMetrics;
use crate::infra::plugin::NullCredentialResolver;
use crate::infra::proxy::{PluginExecutor, ProxyEngine};
use crate::infra::ratelimit::RateLimiter;
use crate::infra::storage::InMemoryStore;
use crate::infra::type_provisioning;

/// Path the documented public surface is mounted under (DESIGN §3.2).
///
/// Gear routes are registered relative to the gear (`/oagw/v1/…`); this prefix
/// adds the `/api/oagw/v1/…` spelling the API contract publishes.
const API_MOUNT: &str = "/api";

/// Concrete management service the gear wires.
pub type ConcreteManagement = ManagementService<InMemoryStore, InMemoryStore, InMemoryStore>;

/// Concrete proxy service the gear wires.
pub type ConcreteProxy =
    ProxyService<InMemoryStore, InMemoryStore, FlatTenantHierarchy, RateLimiter>;

/// The outbound API gateway gear.
#[toolkit::gear(name = "oagw", deps = [types_registry, credstore], capabilities = [rest])]
pub struct Oagw {
    management: OnceLock<Arc<ConcreteManagement>>,
    proxy: OnceLock<Arc<ConcreteProxy>>,
    engine: OnceLock<Arc<ProxyEngine>>,
    config: OnceLock<Arc<OagwConfig>>,
    metrics: OnceLock<Arc<OagwMetrics>>,
}

impl Default for Oagw {
    fn default() -> Self {
        Self {
            management: OnceLock::new(),
            proxy: OnceLock::new(),
            engine: OnceLock::new(),
            config: OnceLock::new(),
            metrics: OnceLock::new(),
        }
    }
}

impl Oagw {
    /// The management service, after [`Gear::init`].
    #[must_use]
    pub fn management(&self) -> Option<Arc<ConcreteManagement>> {
        self.management.get().cloned()
    }

    /// The proxy service, after [`Gear::init`].
    #[must_use]
    pub fn proxy(&self) -> Option<Arc<ConcreteProxy>> {
        self.proxy.get().cloned()
    }

    /// The proxy engine, after [`Gear::init`].
    #[must_use]
    pub fn engine(&self) -> Option<Arc<ProxyEngine>> {
        self.engine.get().cloned()
    }

    /// The effective gear configuration, after [`Gear::init`].
    #[must_use]
    pub fn config(&self) -> Option<Arc<OagwConfig>> {
        self.config.get().cloned()
    }

    /// The data-plane metrics, after [`Gear::init`].
    #[must_use]
    pub fn metrics(&self) -> Option<Arc<OagwMetrics>> {
        self.metrics.get().cloned()
    }

    /// Builds the data-plane stack the REST layer extends.
    fn proxy_stack(&self) -> anyhow::Result<ProxyStack> {
        let service = self
            .proxy
            .get()
            .ok_or_else(|| anyhow::anyhow!("{} proxy service not initialized", Self::MODULE_NAME))?
            .clone();
        let engine = self
            .engine
            .get()
            .ok_or_else(|| anyhow::anyhow!("{} proxy engine not initialized", Self::MODULE_NAME))?
            .clone();
        let config = self
            .config
            .get()
            .ok_or_else(|| anyhow::anyhow!("{} config not initialized", Self::MODULE_NAME))?
            .clone();
        let metrics = self
            .metrics
            .get()
            .ok_or_else(|| anyhow::anyhow!("{} metrics not initialized", Self::MODULE_NAME))?
            .clone();
        Ok(ProxyStack {
            service,
            engine,
            config,
            metrics,
        })
    }
}

#[async_trait]
impl Gear for Oagw {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let config: OagwConfig = ctx
            .config_or_default()
            .map_err(|error| anyhow::anyhow!("failed to read oagw config: {error}"))?;
        let config = Arc::new(config);

        let store = Arc::new(InMemoryStore::default());
        let routes_store = Arc::new(InMemoryStore::default());
        let plugins_store = Arc::new(InMemoryStore::default());
        let limiter = Arc::new(RateLimiter::new());

        let management = Arc::new(ManagementService::new(
            store.clone(),
            routes_store.clone(),
            plugins_store.clone(),
            config.clone(),
        ));
        self.management
            .set(management)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        let proxy = Arc::new(ProxyService::new(
            store,
            routes_store,
            Arc::new(FlatTenantHierarchy),
            limiter,
            config.clone(),
        ));
        self.proxy
            .set(proxy)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        // Credential material always comes from the CredStore; a build without
        // the gear degrades to a resolver that never yields a secret.
        let resolver: Arc<dyn crate::infra::plugin::CredentialResolver> =
            match ctx
                .client_hub()
                .get::<dyn credstore_sdk::CredStoreClientV1>()
            {
                Ok(store) => {
                    info!("OAGW resolves credential references through the CredStore");
                    Arc::new(crate::infra::credentials::CredStoreResolver::new(store))
                }
                Err(_) => {
                    tracing::warn!(
                        "CredStore client unavailable; OAGW auth plugins resolve no secrets"
                    );
                    Arc::new(NullCredentialResolver)
                }
            };

        let executor = Arc::new(PluginExecutor::new(
            resolver,
            std::time::Duration::from_secs(config.token_cache_ttl_secs.max(1)),
            config.token_cache_capacity,
        ));

        let metrics = Arc::new(OagwMetrics::default());
        self.metrics
            .set(Arc::clone(&metrics))
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        let client = OutboundClient::new(
            config.proxy_timeout(),
            config.connect_timeout(),
            std::time::Duration::from_secs(90),
            128,
        )?;
        let engine = Arc::new(
            ProxyEngine::new(client, executor, config.clone()).with_metrics(Arc::clone(&metrics)),
        );
        self.engine
            .set(engine)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        self.config
            .set(config)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        // Publish the OAGW type vocabulary; a failure is never fatal.
        let registry = ctx
            .client_hub()
            .get::<dyn types_registry_sdk::TypesRegistryClient>()
            .ok();
        type_provisioning::provision(registry.as_deref()).await;

        info!("OAGW gear initialized");
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
        info!("Registering OAGW management and proxy routes");

        let stack = self.proxy_stack()?;
        let management = self
            .management
            .get()
            .ok_or_else(|| {
                anyhow::anyhow!("{} management service not initialized", Self::MODULE_NAME)
            })?
            .clone();

        // Each plane gets its own extension so no route sees the other's state.
        let management_router = routes::register_routes(axum::Router::new(), openapi)
            .layer(axum::Extension(management));
        let proxy_router = axum::Router::new()
            .route(
                &format!("{PROXY_PREFIX}/{{*tail}}"),
                axum::routing::any(proxy_handler),
            )
            .layer(axum::Extension(stack));

        let gear_relative = management_router.clone().merge(proxy_router.clone());

        // DESIGN §3.2 publishes the control and data planes at `/api/oagw/v1/…`.
        // Routes are registered gear-relative (`/oagw/v1/…`), which is what the
        // OpenAPI document and the gateway's auth matching are keyed on, so the
        // documented surface is mounted as an additional nested copy under
        // `api/`. The copy rides the same handlers and extensions.
        let gear_relative = router.clone().merge(gear_relative);
        let documented = gear_relative.clone().nest(API_MOUNT, gear_relative.clone());

        info!("OAGW REST routes registered successfully");
        Ok(documented)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_gear_publishes_nothing() {
        let gear = Oagw::default();
        assert!(gear.management().is_none());
        assert!(gear.proxy().is_none());
        assert!(gear.engine().is_none());
        assert!(gear.config().is_none());
        assert!(gear.metrics().is_none());
    }

    #[test]
    fn the_proxy_route_is_gear_relative() {
        assert_eq!(PROXY_PREFIX, "/oagw/v1/proxy");
        assert_eq!(
            format!("{PROXY_PREFIX}/{{*tail}}"),
            "/oagw/v1/proxy/{*tail}"
        );
    }
}
