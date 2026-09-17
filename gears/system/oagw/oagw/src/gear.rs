// Created: 2026-09-03 by Constructor Tech
//! Gear declaration of the Outbound API Gateway.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use toolkit::GearCtx;
use toolkit::api::OpenApiRegistry;
use toolkit::{Gear, RestApiCapability};
use tracing::info;

use crate::config::OagwConfig;
use crate::state::OagwState;

/// Outbound API Gateway gear.
///
/// Owns the control plane (CRUD of upstreams, routes and plugin definitions)
/// and the data plane (the `/oagw/v1/proxy/{alias}` reverse proxy).
///
/// ## Capabilities
///
/// - `system` — Core infrastructure gear, initialized early in startup
/// - `rest` — Exposes REST API endpoints
///
/// ## Dependencies
///
/// - `credstore` — resolves `cred://` references in auth configurations.
/// - `types_registry` — validates the GTS identifiers carried by resources.
/// - `tenant_resolver` — walks the ancestor chain of the calling tenant.
/// - `authz_resolver` — authorization of the caller, enforced by the toolkit
///   REST layer before a handler runs.
#[toolkit::gear(
    name = "oagw",
    deps = [credstore, types_registry, tenant_resolver, authz_resolver],
    capabilities = [system, rest]
)]
pub struct OagwGear {
    state: OnceLock<Arc<OagwState>>,
}

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            state: OnceLock::new(),
        }
    }
}

impl OagwGear {
    /// The shared runtime state, available once `init` has run.
    #[must_use]
    pub fn state(&self) -> Option<Arc<OagwState>> {
        self.state.get().cloned()
    }
}

#[async_trait]
impl Gear for OagwGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let config: OagwConfig = ctx.config_or_default()?;
        info!(
            allow_http_upstream = config.allow_http_upstream,
            proxy_timeout_secs = config.proxy_timeout_secs,
            idle_timeout_secs = config.idle_timeout_secs,
            max_body_bytes = config.max_body_bytes,
            "Loaded oagw config"
        );

        let hub = ctx.client_hub();
        let credstore = hub.try_get::<dyn credstore_sdk::CredStoreClientV1>();
        let tenant_resolver = hub.try_get::<dyn tenant_resolver_sdk::TenantResolverClient>();
        if credstore.is_none() {
            tracing::warn!("credstore client unavailable; cred:// references will fail");
        }
        if tenant_resolver.is_none() {
            tracing::warn!(
                "tenant resolver client unavailable; upstreams resolve for the calling tenant only"
            );
        }

        let _ = self
            .state
            .set(Arc::new(OagwState::with_clients(config, credstore, tenant_resolver)));
        info!("oagw gear initialized");
        Ok(())
    }
}

impl toolkit::contracts::SystemCapability for OagwGear {}

impl RestApiCapability for OagwGear {
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        let state = self
            .state
            .get()
            .ok_or_else(|| anyhow::anyhow!("oagw gear state is not initialized"))?
            .clone();
        info!("Registering oagw REST routes");
        let routes = crate::api::full_router(state, openapi)?;
        Ok(router.merge(routes))
    }
}
