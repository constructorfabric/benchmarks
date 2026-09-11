//! Gear declaration for the OAGW (outbound API gateway) gear.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use tenant_resolver_sdk::TenantResolverClient;
use toolkit::api::OpenApiRegistry;
use toolkit::{Gear, GearCtx, RestApiCapability};
use tracing::info;

use crate::config::OagwConfig;
use crate::domain::store::Store;
use crate::proxy::ProxyState;

/// The OAGW gear.
///
/// Serves the management API and the proxy data plane under `/oagw/v1`.
///
/// ## Capabilities
///
/// - `rest` — the management API and the proxy catch-all
///
/// ## Dependencies
///
/// - `credstore` — resolves `cred://` secret references for the auth plugins
/// - `tenant-resolver` — walks the tenant hierarchy for inherited configuration
#[toolkit::gear(
    name = "oagw",
    deps = [credstore, tenant_resolver],
    capabilities = [rest]
)]
pub struct OagwGear {
    state: OnceLock<Arc<ProxyState>>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            state: OnceLock::new(),
        }
    }
}

#[async_trait]
impl Gear for OagwGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let config: OagwConfig = ctx.config_or_default()?;
        info!(
            proxy_timeout_secs = config.proxy_timeout_secs,
            allow_http_upstream = config.allow_http_upstream,
            ssrf_policy_enabled = config.ssrf_policy.enabled,
            "Loaded oagw config"
        );

        // Fail-closed: the auth plugins resolve `cred://` references through the
        // credential store, so without it no authenticated upstream call can be
        // made (credstore is a hard `deps` and initializes first).
        let credstore = ctx
            .client_hub()
            .get::<dyn credstore_sdk::CredStoreClientV1>()
            .map_err(|e| anyhow::anyhow!("failed to get CredStoreClientV1: {e}"))?;

        // The tenant resolver is what makes hierarchical configuration (sharing,
        // shadowing, ancestor enforcement) possible; without it every tenant is
        // treated as its own root.
        let tenant_resolver = ctx
            .client_hub()
            .get::<dyn TenantResolverClient>()
            .map_err(|e| anyhow::anyhow!("failed to get TenantResolverClient: {e}"))?;

        let state = Arc::new(ProxyState::new(
            Arc::new(Store::new()),
            config,
            credstore,
            Some(tenant_resolver),
        ));

        self.state
            .set(state)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        info!("oagw gear initialized");
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
            .ok_or_else(|| anyhow::anyhow!("ProxyState not initialized"))?
            .clone();

        let router = crate::api::register_routes(router, openapi, state);

        info!("OAGW REST routes registered successfully");
        Ok(router)
    }
}
