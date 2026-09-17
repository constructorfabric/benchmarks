//! Gear declaration for the OAGW gear.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::SystemCapability;
use toolkit::{Gear, GearCtx};
use tracing::{debug, info};

use crate::config::OagwConfig;
use crate::domain::plugin::TokenCacheConfig;
use crate::domain::service::{ControlPlaneService, DataPlaneService};
use crate::infra::proxy::ProxyEngine;
use crate::infra::store::Store;

/// OAGW gear: outbound API gateway (control plane + data plane).
///
/// ## Capabilities
///
/// - `system` — initialized early so the proxy surface exists before traffic
/// - `rest` — exposes `/oagw/v1/...`
#[toolkit::gear(
    name = "oagw",
    deps = [credstore, tenant_resolver],
    capabilities = [system, rest]
)]
pub struct OagwGear {
    control: OnceLock<Arc<ControlPlaneService>>,
    data: OnceLock<Arc<DataPlaneService>>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            control: OnceLock::new(),
            data: OnceLock::new(),
        }
    }
}

#[async_trait]
impl SystemCapability for OagwGear {}

#[async_trait]
impl Gear for OagwGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let config: OagwConfig = ctx.config_or_default()?;
        debug!(
            proxy_timeout_secs = config.proxy_timeout_secs,
            allow_http_upstream = config.allow_http_upstream,
            ssrf_policy = config.ssrf_policy.enabled,
            token_cache_ttl_secs = config.token_cache_ttl_secs,
            token_cache_capacity = config.token_cache_capacity,
            "Loaded oagw config"
        );

        let store = Arc::new(Store::new());
        let engine = Arc::new(
            ProxyEngine::new(config.clone())
                .map_err(|error| anyhow::anyhow!("oagw proxy engine unavailable: {error}"))?,
        );
        let guards = Arc::new(crate::domain::plugin::GuardPluginRegistry::with_builtins());
        let transforms = Arc::new(crate::domain::plugin::TransformPluginRegistry::with_builtins());

        // Sibling-gear integrations. `credstore` backs the auth plugins (API
        // key and OAuth2 client credentials both dereference a `cred://`
        // pointer); the tenant resolver backs alias shadowing. A deployment
        // that does not ship the credential store still gets a working
        // gateway, minus credential-backed auth.
        let credstore: Option<Arc<dyn credstore_sdk::CredStoreClientV1>> =
            ctx.client_hub()
                .try_get::<dyn credstore_sdk::CredStoreClientV1>();
        let credstore = credstore.unwrap_or_else(|| {
            info!(
                "oagw: no credstore client registered; credential-backed auth plugins fail closed"
            );
            Arc::new(crate::infra::secrets::NullCredStore)
        });
        let auth_plugins = Arc::new(crate::domain::plugin::AuthPluginRegistry::with_builtins(
            credstore,
            TokenCacheConfig {
                ttl: config.token_cache_ttl(),
                capacity: config.token_cache_capacity,
            },
        ));

        let tenant_resolver: Arc<dyn tenant_resolver_sdk::TenantResolverClient> = ctx
            .client_hub()
            .get::<dyn tenant_resolver_sdk::TenantResolverClient>()
            .map_err(|error| anyhow::anyhow!("oagw requires the tenant resolver: {error}"))?;

        let control = Arc::new(ControlPlaneService::new(
            store.clone(),
            tenant_resolver.clone(),
            auth_plugins.clone(),
            guards.clone(),
            transforms.clone(),
        ));
        let data = Arc::new(DataPlaneService::new(
            config,
            store,
            engine,
            auth_plugins,
            guards,
            transforms,
            tenant_resolver,
        ));

        self.control
            .set(control)
            .map_err(|_| anyhow::anyhow!("oagw gear already initialized"))?;
        self.data
            .set(data)
            .map_err(|_| anyhow::anyhow!("oagw gear already initialized"))?;

        info!("oagw gear initialized");
        Ok(())
    }
}

impl OagwGear {
    /// The control-plane service, after `Gear::init`.
    #[must_use]
    pub fn control_plane(&self) -> Option<Arc<ControlPlaneService>> {
        self.control.get().cloned()
    }

    /// The data-plane service.
    #[must_use]
    pub fn data_plane(&self) -> Option<Arc<DataPlaneService>> {
        self.data.get().cloned()
    }
}

impl toolkit::RestApiCapability for OagwGear {
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        let control = self
            .control
            .get()
            .ok_or_else(|| anyhow::anyhow!("oagw control plane not initialized"))?
            .clone();
        let data = self
            .data
            .get()
            .ok_or_else(|| anyhow::anyhow!("oagw data plane not initialized"))?
            .clone();
        let router = crate::api::routes::register_routes(router, openapi, control, data);
        info!("oagw REST routes registered");
        Ok(router)
    }
}
