//! Gear declaration for the `oagw` gear (outbound API gateway).
//!
//! ## Capabilities
//!
//! - `rest` — the management plane at `/oagw/v1/...` *and* the proxy data
//!   plane at `/oagw/v1/proxy/{*proxy_path}`.
//!
//! ## State
//!
//! Both planes share one `ConfigStore` behind an [`OnceLock`], so an upstream
//! the management plane just wrote is visible to the very next proxy hop. The
//! data plane additionally needs the `credstore` and `tenant-resolver`
//! clients, which are resolved from the platform's client hub during `init`.
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use toolkit::api::OpenApiRegistry;
use toolkit::{Gear, GearCtx, RestApiCapability};
use tracing::info;

use crate::config::OagwConfig;
use crate::domain::services::{ControlPlane, ListLimits};
use crate::infra::memory::MemoryStore;
use crate::infra::proxy::{ConfigSource, DataPlane, UpstreamDialer};

/// The `oagw` gear.
#[toolkit::gear(
    name = "oagw",
    deps = [credstore, types_registry, authz_resolver, tenant_resolver],
    capabilities = [rest]
)]
pub struct OagwGear {
    control_plane: OnceLock<Arc<ControlPlane>>,
    data_plane: OnceLock<Arc<DataPlane>>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            control_plane: OnceLock::new(),
            data_plane: OnceLock::new(),
        }
    }
}

impl OagwGear {
    /// The control plane installed during `init`, if the gear is initialized.
    #[must_use]
    pub fn control_plane(&self) -> Option<Arc<ControlPlane>> {
        self.control_plane.get().cloned()
    }

    /// The proxy data plane installed during `init`, if the gear is
    /// initialized.
    #[must_use]
    pub fn data_plane(&self) -> Option<Arc<DataPlane>> {
        self.data_plane.get().cloned()
    }
}

#[async_trait]
impl Gear for OagwGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let config: OagwConfig = ctx.config_or_default()?;
        config
            .validate()
            .map_err(|violation| anyhow::anyhow!("oagw: invalid configuration: {violation}"))?;
        info!(
            proxy_timeout_secs = config.proxy_timeout_secs,
            allow_http_upstream = config.allow_http_upstream,
            ssrf_policy_enabled = config.ssrf_policy.enabled,
            list_default_top = config.list.default_top,
            list_max_top = config.list.max_top,
            "Loaded oagw configuration"
        );

        // One store, two planes: the proxy reads what the management plane
        // writes, with no cache in between.
        let store = Arc::new(MemoryStore::new());
        let control_plane = Arc::new(ControlPlane::new(
            store.clone(),
            ListLimits {
                default_top: config.list.default_top,
                max_top: config.list.max_top,
            },
        ));
        let data_plane = build_data_plane(ctx, &config, store);
        self.control_plane
            .set(control_plane)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
        self.data_plane
            .set(data_plane)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        info!("oagw management plane and proxy data plane initialized");
        Ok(())
    }
}

/// Assemble the proxy data plane over the shared store.
fn build_data_plane(ctx: &GearCtx, config: &OagwConfig, store: Arc<MemoryStore>) -> Arc<DataPlane> {
    let secrets: Arc<dyn crate::domain::plugin::SecretResolver> =
        match ctx
            .client_hub()
            .try_get::<dyn credstore_sdk::CredStoreClientV1>()
        {
            Some(client) => Arc::new(crate::infra::plugin::CredStoreSecretResolver::new(client)),
            None => {
                info!(
                    "oagw: no credstore client registered; secret-backed auth plugins are unavailable"
                );
                Arc::new(crate::infra::plugin::resolver_that_fails())
            }
        };
    let resolver = ctx
        .client_hub()
        .try_get::<dyn tenant_resolver_sdk::TenantResolverClient>();
    let source = ConfigSource::new(
        store,
        Arc::new(crate::infra::proxy::ResolverChain::new(resolver)),
    );
    let dialer = UpstreamDialer::new(
        Arc::new(pingora_core::connectors::TransportConnector::new(None)),
        crate::infra::proxy::SsrfPolicy {
            enabled: config.ssrf_policy.enabled,
        },
        config.allow_http_upstream,
    );
    let engine = crate::infra::plugin::PluginEngine::with_builtins(secrets);
    let timeout = crate::infra::proxy::response::exchange_timeout(config.proxy_timeout_secs);
    Arc::new(DataPlane::new(source, dialer, engine, timeout))
}

impl RestApiCapability for OagwGear {
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        info!("Registering oagw management and proxy routes");

        let control_plane = self
            .control_plane
            .get()
            .ok_or_else(|| anyhow::anyhow!("control plane not initialized"))?
            .clone();
        let data_plane = self
            .data_plane
            .get()
            .ok_or_else(|| anyhow::anyhow!("data plane not initialized"))?
            .clone();

        let router = crate::api::rest::routes::register_routes(router, openapi, control_plane);
        let router = router.layer(axum::Extension(data_plane));

        info!("oagw routes registered successfully");
        Ok(router)
    }
}
