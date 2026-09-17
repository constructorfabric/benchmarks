//! Gear wiring: configuration, service graph and REST registration.

use std::sync::Arc;

use async_trait::async_trait;
use toolkit::api::OpenApiRegistry;
use toolkit::{Gear, GearCtx, RestApiCapability};

use crate::api::rest::routes;
use crate::config::OagwConfig;
use crate::domain::plugin::PluginRegistry;
use crate::domain::repo::Repositories;
use crate::domain::services::{ProxyService, RouteService, UpstreamService};
use crate::infra::cache::ConfigCache;
use crate::infra::plugin::builtin::register_builtins;
use crate::infra::storage::memory::InMemoryRepositories;
use credstore_sdk::CredStoreClientV1;

/// Shared service bundle handed to the REST handlers.
#[derive(Clone)]
pub struct OagwState {
    /// Upstream CRUD.
    pub upstreams: UpstreamService,
    /// Route CRUD.
    pub routes: RouteService,
    /// Data-plane proxy.
    pub proxy: ProxyService,
    /// Effective gear configuration.
    pub config: Arc<OagwConfig>,
}

/// OAGW — outbound API gateway.
///
/// Registers the management API (`/oagw/v1/upstreams`, `/oagw/v1/routes`) and
/// the data-plane proxy (`/oagw/v1/proxy/{*rest}`) under the `rest`
/// capability.
#[toolkit::gear(name = "oagw", capabilities = [rest])]
pub struct OagwGear {
    state: std::sync::OnceLock<Arc<OagwState>>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            state: std::sync::OnceLock::new(),
        }
    }
}

impl OagwGear {
    /// Pre-built state, for tests and embedding.
    #[must_use]
    pub fn with_state(state: Arc<OagwState>) -> Self {
        let gear = Self::default();
        let _ = gear.state.set(state);
        gear
    }

    /// Effective configuration, once initialised.
    #[must_use]
    pub fn config(&self) -> Option<Arc<OagwConfig>> {
        self.state.get().map(|s| s.config.clone())
    }
}

#[async_trait]
impl Gear for OagwGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let config = ctx.config_or_default::<OagwConfig>()?;

        let mut plugins = PluginRegistry::new();
        let credentials = register_builtins(&mut plugins);

        // The L1 configuration cache is shared by the three services, so a
        // management write invalidates the data plane's copy in the same tick
        // (ADR 0005).
        let cache = ConfigCache::new(config.l1_cache_ttl(), config.l1_cache_capacity);
        let repos: Repositories = InMemoryRepositories::new().into_repos();
        let state = Arc::new(OagwState {
            upstreams: UpstreamService::new(&repos).with_cache(Arc::clone(&cache)),
            routes: RouteService::new(&repos).with_cache(Arc::clone(&cache)),
            proxy: ProxyService::try_new(repos, config.clone(), plugins)
                .map_err(|e| anyhow::anyhow!("oagw proxy service: {e}"))?
                .with_config_cache(cache)
                .with_credentials(Arc::clone(&credentials)),
            config: Arc::new(config.clone()),
        });

        self.state
            .set(state)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        let initialized = self
            .state
            .get()
            .ok_or_else(|| anyhow::anyhow!("OagwState unavailable after init"))?;

        // Wire the credstore client into the shared credential store, so the
        // auth plugins can resolve `cred://` references (ADR 0008).
        match ctx.client_hub().get::<dyn CredStoreClientV1>() {
            Ok(client) => initialized.proxy.credentials().set_client(client),
            Err(error) => {
                tracing::warn!(
                    %error,
                    "credstore client unavailable; 'cred://' references will fail closed"
                );
            }
        }

        // Phase-2 seam: provision the OAGW types into the types-registry.
        initialized.proxy.provision_types()?;

        tracing::info!(
            proxy_timeout_secs = initialized.config.proxy_timeout_secs,
            allow_http_upstream = initialized.config.allow_http_upstream,
            ssrf_enabled = initialized.config.ssrf_policy.enabled,
            "OAGW gear initialized"
        );
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
        tracing::info!("Registering OAGW REST routes");

        let state = self
            .state
            .get()
            .ok_or_else(|| anyhow::anyhow!("OagwState not initialized"))?
            .clone();

        let router = routes::register_routes(router, openapi, state);
        tracing::info!("OAGW REST routes registered successfully");
        Ok(router)
    }
}
