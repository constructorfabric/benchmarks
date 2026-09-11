// Created: 2026-09-01 by Constructor Tech
//! The `oagw` gear.
//!
//! `docs/DESIGN.md` §2: a Control Plane (the management API) and a Data
//! Plane (the proxy), both served from the same in-memory store. The gear
//! registers the `rest` capability only — `docs/DESIGN.md` §3.6 notes that
//! its `Cargo.toml` carries no `toolkit-db` dependency, so state lives in
//! process memory and is reset by a restart, which is the documented
//! single-exec deployment mode.

use std::sync::Arc;

use async_trait::async_trait;
use credstore_sdk::CredStoreClientV1;
use tenant_resolver_sdk::TenantResolverClient;
use toolkit::Gear;
use toolkit::RestApiCapability;
use toolkit::context::GearCtx;
use tracing::info;

use crate::api::routes;
use crate::api::state::OagwState;
use crate::config::OagwConfig;
use crate::domain::store::Store;
use crate::infra::dp::{DataPlane, DpConfig};
use crate::infra::plugin::Registries;
use crate::infra::tenant::TenantChain;

/// The outbound API gateway.
#[toolkit::gear(
    name = "oagw",
    deps = [credstore, tenant_resolver, types_registry],
    capabilities = [rest]
)]
pub struct Oagw {
    state: std::sync::OnceLock<OagwState>,
}

impl Default for Oagw {
    fn default() -> Self {
        Self {
            state: std::sync::OnceLock::new(),
        }
    }
}

#[async_trait]
impl Gear for Oagw {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let config: OagwConfig = ctx.config_or_default()?;
        info!(
            proxy_timeout_secs = config.proxy_timeout_secs,
            allow_http_upstream = config.allow_http_upstream,
            "initializing the oagw gear"
        );

        let store: crate::domain::store::SharedStore = Arc::new(Store::new());

        // Credential resolution: `cred://` references go to cred_store.
        let secrets = match ctx.client_hub().get::<dyn CredStoreClientV1>() {
            Ok(client) => crate::infra::credstore::SecretResolver::new(client),
            Err(error) => {
                info!(error = %error, "credstore is unavailable; every secret lookup fails");
                crate::infra::credstore::SecretResolver::unlinked()
            }
        };

        // Tenant hierarchy: the data plane walks the ancestor chain.
        let tenants = match ctx.client_hub().get::<dyn TenantResolverClient>() {
            Ok(client) => TenantChain::new(client),
            Err(error) => {
                info!(error = %error, "tenant-resolver is unavailable; every tenant stands alone");
                TenantChain::unlinked()
            }
        };

        let registries = Registries::builtins(
            secrets.clone(),
            None,
            crate::infra::plugin::oauth2::TokenCacheConfig {
                ttl: std::time::Duration::from_secs(config.token_cache_ttl_secs),
                capacity: config.token_cache_capacity,
            },
        );

        let dp = DataPlane::new(DpConfig {
            ttfb: std::time::Duration::from_secs(config.proxy_timeout_secs),
            allow_http_upstream: config.allow_http_upstream,
            max_body_size: config.max_body_size_bytes,
            breaker: crate::domain::breaker::Thresholds {
                failure_threshold: config.circuit_breaker.failure_threshold,
                window: std::time::Duration::from_secs(config.circuit_breaker.window_secs),
                open: std::time::Duration::from_secs(config.circuit_breaker.open_secs),
            },
            ws_idle_timeout: std::time::Duration::from_secs(config.ws_idle_timeout_secs),
        })
        .map_err(|error| anyhow::anyhow!("oagw data plane unavailable: {error}"))?;

        let state = OagwState {
            store,
            dp: Arc::new(dp),
            registries,
            secrets,
            tenants,
            config: Arc::new(config),
        };
        self.state
            .set(state)
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        info!("oagw gear initialized");
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
        info!("registering oagw REST routes");
        let state = self
            .state
            .get()
            .ok_or_else(|| anyhow::anyhow!("{} gear is not initialized", Self::MODULE_NAME))?
            .clone();

        // Registered on a private sub-router so the `Extension` layer
        // carrying the state is applied to oagw's routes only.
        let scoped = routes::register(axum::Router::new(), openapi).layer(axum::Extension(state));
        Ok(router.merge(scoped))
    }
}
