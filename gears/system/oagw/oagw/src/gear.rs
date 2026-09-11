//! Gear declaration for the OAGW gear (`DESIGN.md` § 2).

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use toolkit::api::OpenApiRegistry;
use toolkit::{Gear, GearCtx, RestApiCapability};
use tracing::{debug, info};

use crate::config::OagwConfig;
use crate::domain::services::management::{ManagementService, SelfChain};
use crate::infra::metrics::ProxyMetrics;
use crate::infra::plugins::registry::PluginRegistry;
use crate::infra::proxy::ProxyService;
use crate::infra::storage::ControlPlaneStore;

/// OAGW — the outbound API gateway.
///
/// ## Capabilities
///
/// - `rest` — the management plane (`/oagw/v1/{upstreams,routes,plugins}`) and
///   the proxy data plane (`/oagw/v1/proxy/{alias}[/{path_suffix}]`).
///
/// ## Dependencies
///
/// - `credstore` — the OAuth2 client-credentials plugins resolve a
///   `secret_ref` through it (`ADR/0008`). The dependency is soft: when the
///   gear is absent the plugins fail the request with the documented
///   `credential` error instead of failing startup.
#[toolkit::gear(
    name = "oagw",
    capabilities = [rest],
    deps = [credstore]
)]
pub struct OagwGear {
    management: OnceLock<Arc<ManagementService>>,
    proxy: OnceLock<Arc<ProxyService>>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            management: OnceLock::new(),
            proxy: OnceLock::new(),
        }
    }
}

#[async_trait]
impl Gear for OagwGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let config: OagwConfig = ctx.config_or_default()?;
        config
            .validate()
            .map_err(|e| anyhow::anyhow!("oagw config: {e}"))?;
        debug!(
            proxy_timeout_secs = config.proxy_timeout_secs,
            allow_http_upstream = config.allow_http_upstream,
            max_body_bytes = config.max_body_bytes,
            "Loaded oagw config"
        );

        let store = Arc::new(ControlPlaneStore::new());
        let chain = Arc::new(SelfChain);
        let registry = Arc::new(PluginRegistry::with_builtins(
            config.token_cache_ttl(),
            config.token_cache_capacity,
        ));
        let metrics = ProxyMetrics::new();

        let credstore = ctx
            .client_hub()
            .try_get::<dyn credstore_sdk::CredStoreClientV1>();

        let store: Arc<dyn crate::domain::repo::ControlPlane> = store;
        let chain: Arc<dyn crate::domain::services::management::TenantChain> = chain;
        let management = Arc::new(ManagementService::new(
            Arc::clone(&store),
            Arc::clone(&chain),
            config.allow_http_upstream,
        ));
        let proxy = Arc::new(
            ProxyService::new(store, chain, registry, config, metrics)?.with_credstore(credstore),
        );

        self.management
            .set(management)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
        self.proxy
            .set(proxy)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        info!(
            plugin_impls = crate::infra::type_provisioning::declared_type_schemas().len(),
            "oagw gear initialized"
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
        info!("Registering oagw REST routes");

        let management = self
            .management
            .get()
            .ok_or_else(|| anyhow::anyhow!("Management service not initialized"))?
            .clone();
        let proxy = self
            .proxy
            .get()
            .ok_or_else(|| anyhow::anyhow!("Proxy service not initialized"))?
            .clone();

        let router = crate::api::rest::routes::register_routes(router, openapi, management, proxy);

        info!("OAGW REST routes registered successfully");
        Ok(router)
    }
}
