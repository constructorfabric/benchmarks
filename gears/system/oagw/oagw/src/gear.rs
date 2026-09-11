// Created: 2026-09-02 by Constructor Tech
//! Gear entry point: wires the control plane, the data plane and the REST
//! surface into the host runtime.
//!
//! The gateway owns no database — its roster lives in memory — so the only
//! capability it asks for is `rest`. Everything else it needs (the credential
//! store, the tenant resolver) comes from the [`GearCtx`]'s client hub and is
//! optional: a deployment that does not register them still boots, with secret
//! references unresolvable and tenant hierarchies treated as flat.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use toolkit::api::OpenApiRegistry;
use toolkit::{Gear, GearCtx};
use tracing::info;

use crate::api::handlers::{ManagementState, ProxyState};
use crate::api::routes;
use crate::config::OagwConfig;
use crate::domain::plugin::{AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry};
use crate::domain::service::ControlPlane;
use crate::domain::store::Store;
use crate::infra::client::{CredStoreSecretResolver, NoopSecretResolver, UpstreamClient};
use crate::infra::proxy::ProxyService;

/// The assembled gateway: control plane on one side, proxy on the other.
#[toolkit::gear(name = "oagw", capabilities = [rest])]
pub struct Oagw {
    config: OnceLock<OagwConfig>,
    management: OnceLock<ManagementState>,
    proxy: OnceLock<ProxyState>,
}

impl Default for Oagw {
    fn default() -> Self {
        Self {
            config: OnceLock::new(),
            management: OnceLock::new(),
            proxy: OnceLock::new(),
        }
    }
}

impl Oagw {
    /// The configuration the gear booted with.
    ///
    /// # Errors
    ///
    /// When the gear has not been initialized.
    pub fn config(&self) -> anyhow::Result<&OagwConfig> {
        self.config
            .get()
            .ok_or_else(|| anyhow::anyhow!("{} gear is not initialized", Self::MODULE_NAME))
    }

    /// The management-plane state, ready for the REST layer.
    fn management(&self) -> anyhow::Result<ManagementState> {
        self.management
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("{} management state is not initialized", Self::MODULE_NAME))
    }

    /// The data-plane state, ready for the REST layer.
    fn proxy(&self) -> anyhow::Result<ProxyState> {
        self.proxy
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("{} proxy state is not initialized", Self::MODULE_NAME))
    }
}

#[async_trait]
impl Gear for Oagw {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let config: OagwConfig = ctx.config_or_default()?;
        info!(
            allow_http_upstream = config.allow_http_upstream,
            proxy_timeout_secs = config.proxy_timeout_secs,
            ssrf = config.ssrf_policy.enabled,
            "initializing the outbound API gateway"
        );

        // The credential store is optional: a deployment without one still
        // boots, and every `secret_ref` resolves to "unreadable".
        let secrets: Arc<dyn crate::domain::plugin::SecretResolver> =
            match ctx.client_hub().get::<dyn credstore_sdk::CredStoreClientV1>() {
                Ok(client) => Arc::new(CredStoreSecretResolver::new(client)),
                Err(_) => Arc::new(NoopSecretResolver),
            };

        // The tenant resolver is optional too: without it every tenant is
        // standalone and no ancestor roster is ever visible.
        let resolver = ctx
            .client_hub()
            .get::<dyn tenant_resolver_sdk::TenantResolverClient>()
            .ok();

        let store = Arc::new(Store::new());
        let control_plane = Arc::new(
            ControlPlane::new(store, config.list_top_default, config.list_top_max)
                .with_secrets(secrets.clone()),
        );
        let control_plane = match resolver.clone() {
            Some(resolver) => Arc::new(
                (*control_plane).clone().with_tenant_resolver(resolver),
            ),
            None => control_plane,
        };

        let client = UpstreamClient::new(
            config.allow_http_upstream,
            std::time::Duration::from_secs(config.connect_timeout_secs),
            config.ssrf_policy.clone(),
        );
        let auth = AuthPluginRegistry::with_builtins(
            secrets,
            config.token_cache_ttl_secs,
            config.token_cache_capacity,
        );

        let proxy = Arc::new(ProxyService::new(
            control_plane.clone(),
            client,
            auth,
            GuardPluginRegistry::with_builtins(),
            TransformPluginRegistry::with_builtins(),
            config.clone(),
        ));

        self.config
            .set(config)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
        self.management
            .set(ManagementState {
                control_plane,
                resolver,
            })
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
        self.proxy
            .set(ProxyState { proxy })
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        info!("outbound API gateway initialized");
        Ok(())
    }
}

impl toolkit::RestApiCapability for Oagw {
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        info!("registering the outbound API gateway REST routes");
        let router = routes::register_routes(router, openapi, self.management()?, self.proxy()?);
        info!("outbound API gateway REST routes registered");
        Ok(router)
    }
}
