//! Gear declaration for the OAGW gear.
//!
//! Phase 2 registers the management API (`/oagw/v1/...`) on the router; phase 6
//! joins the proxy data plane to it under `/oagw/v1/proxy/{alias}`.
//!
//! ## Capabilities
//!
//! - `system` — core infrastructure gear, initialized early in startup
//! - `rest` — exposes the `/oagw/v1/...` management API

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::SystemCapability;
use toolkit::{ClientHub, Gear, GearCtx, RestApiCapability};
use tracing::{debug, info, warn};

use crate::config::OagwConfig;
use crate::domain::service::Service;
use crate::infra::plugins::{
    PluginCatalog, PluginRegistry, SecretResolver, auth::TokenCacheConfig,
};
use crate::infra::ratelimit::RateLimiter;
use crate::infra::secrets::CredStoreSecretResolver;
use crate::infra::store::Store;

/// Shared gear state, swapped as a whole so that in-flight requests keep a
/// consistent view of the configuration and of the store.
#[derive(Debug, Clone)]
pub struct OagwState {
    /// Validated gear configuration (`gears.oagw.config`).
    pub config: OagwConfig,
    /// In-memory per-tenant upstream/route store.
    pub store: Arc<Store>,
    /// In-process registry of the executable built-in plugins.
    pub plugins: Arc<PluginRegistry>,
    /// Per-tenant catalog of custom (Starlark) plugin definitions.
    pub plugin_catalog: Arc<PluginCatalog>,
    /// In-memory token buckets of the ADR-0003 rate limiter, shared by every
    /// request the proxy data plane serves.
    pub rate_limiter: Arc<RateLimiter>,
}

/// OAGW gear: managed egress for tenant workloads calling external services.
#[toolkit::gear(
    name = "oagw",
    capabilities = [system, rest]
)]
pub struct OagwGear {
    /// Published state; set once by [`Gear::init`] and swapped in place by
    /// later phases (config reload / record changes).
    state: OnceLock<Arc<ArcSwap<OagwState>>>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            state: OnceLock::new(),
        }
    }
}

impl OagwGear {
    /// Returns a handle to the published state, or `None` before `init`.
    #[must_use]
    pub fn state(&self) -> Option<Arc<ArcSwap<OagwState>>> {
        self.state.get().cloned()
    }

    /// Builds the state published by [`Gear::init`].
    fn build_state(config: OagwConfig, client_hub: Arc<ClientHub>) -> Arc<ArcSwap<OagwState>> {
        let store = Arc::new(Store::new());
        // Credentials are resolved per request through the credential store the
        // hub holds; without one every secret-dependent plugin fails closed
        // with a 500 instead of injecting nothing.
        let resolver: Arc<dyn SecretResolver> =
            Arc::new(CredStoreSecretResolver::new(Arc::clone(&client_hub)));
        let token_cache = TokenCacheConfig::new(
            Duration::from_secs(config.token_cache_ttl_secs),
            config.token_cache_capacity,
        );
        let plugins = Arc::new(PluginRegistry::with_builtins(resolver, token_cache));
        Arc::new(ArcSwap::from_pointee(OagwState {
            config,
            store,
            plugins,
            plugin_catalog: Arc::new(PluginCatalog::new()),
            rate_limiter: Arc::new(RateLimiter::with_system_clock()),
        }))
    }
}

#[async_trait]
impl Gear for OagwGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let config: OagwConfig = ctx.config_or_default()?;
        config
            .validate()
            .map_err(|error| anyhow::anyhow!("oagw config invalid: {error}"))?;

        debug!(
            proxy_timeout_secs = config.proxy_timeout_secs,
            allow_http_upstream = config.allow_http_upstream,
            ssrf_enabled = config.ssrf_policy.enabled,
            token_cache_ttl_secs = config.token_cache_ttl_secs,
            token_cache_capacity = config.token_cache_capacity,
            "Loaded oagw config"
        );

        self.state
            .set(Self::build_state(config, ctx.client_hub()))
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        info!("oagw gear initialized (in-memory store)");
        Ok(())
    }
}

#[async_trait]
impl SystemCapability for OagwGear {
    async fn post_init(&self, _sys: &toolkit::runtime::SystemContext) -> anyhow::Result<()> {
        // Nothing to defer to post-init yet: the store is empty and no route
        // is registered. Logging the store size here documents the hook that
        // later phases use to warm derived state.
        if let Some(state) = self.state() {
            let state = state.load_full();
            debug!(
                upstreams = state.store.upstream_count_total(),
                routes = state.store.route_count_total(),
                "oagw post_init"
            );
        } else {
            warn!("oagw post_init ran before init published any state");
        }
        Ok(())
    }
}

impl RestApiCapability for OagwGear {
    fn register_rest(
        &self,
        ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        let Some(swap) = self.state() else {
            return Err(anyhow::anyhow!("oagw REST registration ran before init"));
        };
        let state = swap.load_full();
        let service = Arc::new(Service::new(Arc::clone(&state.store), ctx.client_hub()));
        let router = crate::api::rest::register_routes(router, openapi, Arc::clone(&service));
        let plugins = Arc::new(crate::api::rest::PluginApiState {
            plugins: Arc::clone(&state.plugin_catalog),
            registry: Arc::clone(&state.plugins),
            store: Arc::clone(&state.store),
        });
        let router = crate::api::rest::register_plugin_routes(router, openapi, plugins);
        let proxy_state = Arc::new(crate::api::rest::ProxyState {
            gear: swap,
            service: Arc::clone(&service),
            client_hub: ctx.client_hub(),
            client: crate::infra::http_client::ProxyClient::new(),
        });
        let router = crate::api::rest::register_proxy_routes(router, openapi, proxy_state);
        info!(
            "oagw management API registered under /oagw/v1, proxy data plane under /oagw/v1/proxy"
        );
        Ok(router)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use std::sync::Arc;

    use toolkit::ClientHub;

    use super::OagwGear;
    use crate::config::OagwConfig;

    #[test]
    fn gear_is_constructible_without_arguments() {
        // The `#[toolkit::gear]` macro builds the struct through `Default`.
        let gear = OagwGear::default();
        assert!(gear.state().is_none(), "state is only published by init");
    }

    #[test]
    fn state_carries_the_config_and_an_empty_store() {
        let gear = OagwGear::default();
        let state = OagwGear::build_state(OagwConfig::default(), Arc::new(ClientHub::new()));
        // `build_state` is module-private; exercise it through the same path
        // `init` uses and confirm the published handle is shareable.
        assert!(gear.state.set(state).is_ok());
        assert!(gear.state().is_some());

        let published: Arc<_> = gear.state().expect("published state");
        let snapshot = published.load_full();
        assert_eq!(snapshot.config, OagwConfig::default());
        assert_eq!(snapshot.store.upstream_count_total(), 0);
        assert_eq!(snapshot.store.route_count_total(), 0);
        // The rate limiter is published with the state, so the proxy can consult
        // it without going through the store.
        assert!(snapshot.rate_limiter.is_empty());
    }

    #[test]
    fn module_name_matches_the_gear_name() {
        assert_eq!(OagwGear::MODULE_NAME, "oagw");
    }
}
