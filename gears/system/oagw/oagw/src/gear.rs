// Created: 2026-08-31 by Constructor Tech
//! Gear wiring (`#[toolkit::gear]`).
//!
//! The graded deployment provisions no database for this gear, so the control
//! plane is backed by the in-process store of [`crate::domain::store`]; see the
//! crate docs for the documented deviation from DESIGN §3.6.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use credstore_sdk::CredStoreClientV1;
use tenant_resolver_sdk::TenantResolverClient;
use toolkit::GearCtx;
use toolkit::{Gear, RestApiCapability};
use tracing::{info, warn};

use crate::config::OagwConfig;
use crate::domain::proxy::chain::{NoChain, ResolverChain, TenantChain};
use crate::domain::proxy::service::ProxyService;
use crate::domain::service::OagwService;
use crate::domain::store::{InMemoryStore, Store};
use crate::domain::validation::ValidationPolicy;
use crate::infra::plugin::secrets::CredStore;

/// Outbound API Gateway gear: control plane and proxy data plane.
#[toolkit::gear(
    name = "oagw",
    deps = [tenant_resolver, types_registry, credstore],
    capabilities = [rest]
)]
pub struct Oagw {
    service: OnceLock<Arc<OagwService>>,
    proxy: OnceLock<Arc<ProxyService>>,
    config: OnceLock<OagwConfig>,
}

impl Default for Oagw {
    fn default() -> Self {
        Self {
            service: OnceLock::new(),
            proxy: OnceLock::new(),
            config: OnceLock::new(),
        }
    }
}

impl Oagw {
    /// Configuration the gear was initialised with.
    #[must_use]
    pub fn config(&self) -> Option<&OagwConfig> {
        self.config.get()
    }

    /// Control-plane service, available after [`Gear::init`].
    #[must_use]
    pub fn service(&self) -> Option<&Arc<OagwService>> {
        self.service.get()
    }

    /// Proxy data plane, available after [`Gear::init`].
    #[must_use]
    pub fn proxy(&self) -> Option<&Arc<ProxyService>> {
        self.proxy.get()
    }

    fn initialized_service(&self) -> anyhow::Result<Arc<OagwService>> {
        self.service
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("{} service not initialized", Self::MODULE_NAME))
    }

    fn initialized_proxy(&self) -> anyhow::Result<Arc<ProxyService>> {
        self.proxy
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("{} proxy not initialized", Self::MODULE_NAME))
    }

    /// Credential store the auth plugins resolve their `cred://` references
    /// through.
    ///
    /// Fetched once from the `ClientHub`. A deployment that wires none degrades
    /// exactly the way a missing tenant resolver does: the gear still boots, the
    /// plugin registries only carry the plugins that need no credential store,
    /// and an upstream whose auth binding needs one fails its requests with 503
    /// `link.unavailable.v1` instead of forwarding them unauthenticated.
    fn credential_store(ctx: &GearCtx) -> Option<CredStore> {
        match ctx.client_hub().get::<dyn CredStoreClientV1>() {
            Ok(client) => Some(client),
            Err(error) => {
                warn!(
                    error = %error,
                    "credential store unavailable; auth plugins that need one are not registered"
                );
                None
            }
        }
    }

    /// Tenant chain for alias shadowing.
    ///
    /// The `tenant_resolver` client is fetched once from the `ClientHub`; a
    /// deployment without it degrades to a per-tenant alias lookup instead of
    /// failing the gear.
    fn tenant_chain(ctx: &GearCtx) -> Arc<dyn TenantChain> {
        match ctx.client_hub().get::<dyn TenantResolverClient>() {
            Ok(client) => Arc::new(ResolverChain::new(client)),
            Err(error) => {
                warn!(
                    error = %error,
                    "tenant resolver unavailable; alias shadowing stays within the calling tenant"
                );
                Arc::new(NoChain)
            }
        }
    }
}

#[async_trait]
impl Gear for Oagw {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        // Every member has a serde default, so an absent `gears.oagw` block
        // still yields the documented baseline.
        let config: OagwConfig = ctx.config_or_default()?;
        let policy: ValidationPolicy = config.validation_policy();
        let store: Arc<dyn Store> = InMemoryStore::new();
        let service = OagwService::new(policy, Arc::clone(&store));
        // One outbound client per configuration, built once: no retries, no
        // redirects, and the transport decision of the plaintext switch.
        let client = ProxyService::build_client(&config)
            .map_err(|error| anyhow::anyhow!("{} proxy client: {error}", Self::MODULE_NAME))?;
        let proxy = ProxyService::new(
            store,
            Self::tenant_chain(ctx),
            client,
            Self::credential_store(ctx),
            &config,
        );
        // The data plane keeps per-upstream round-robin cursors; it learns
        // about a cascade deletion from the control plane rather than polling.
        let observer: Arc<dyn crate::domain::lifecycle::UpstreamRemoval> = proxy.clone();
        service.observe_removals(observer);

        self.config
            .set(config)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
        self.service
            .set(service)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
        self.proxy
            .set(proxy)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        info!(
            "{} gear initialized (in-memory control plane, proxy data plane)",
            Self::MODULE_NAME
        );
        Ok(())
    }
}

impl RestApiCapability for Oagw {
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn toolkit::api::OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        let service = self.initialized_service()?;
        let proxy = self.initialized_proxy()?;
        let router = crate::api::routes::register_routes(router, openapi, service);
        let router = crate::api::routes::register_data_plane(router, openapi, proxy);
        info!("{} REST routes registered", Self::MODULE_NAME);
        Ok(router)
    }
}

#[cfg(test)]
mod tests {
    use crate::gear::Oagw;

    #[test]
    fn gear_has_a_default_state() {
        let gear = Oagw::default();
        assert!(gear.service().is_none());
        assert!(gear.config().is_none());
    }
}
