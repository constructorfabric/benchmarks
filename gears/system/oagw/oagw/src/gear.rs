//! Gear declaration: outbound API gateway (OAGW).
//!
//! Capabilities:
//!
//! * `system` — core infrastructure gear (initialized early)
//! * `rest` — exposes the management API and the data-plane proxy endpoint
//!
//! Platform services (credential store, tenant resolver) are adopted
//! best-effort when present in the client hub; the gear remains fully
//! functional without them (tests, resolver-less deployments).

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use toolkit::api::OpenApiRegistry;
use toolkit::contracts::SystemCapability;
use toolkit::{Gear, GearCtx, RestApiCapability};

use crate::config::OagwConfig;
use crate::state::OagwState;

/// Outbound API gateway gear.
#[toolkit::gear(
    name = "oagw",
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

#[async_trait]
impl Gear for OagwGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: OagwConfig = ctx.config_or_default()?;
        tracing::debug!(
            http_allowed = cfg.http_allowed(),
            ssrf_enforced = cfg.ssrf_enforced(),
            "Initializing oagw gear"
        );

        let mut state = OagwState::new(cfg)?;

        // Adopt platform services best-effort: credential store for `cred://`
        // resolution, tenant resolver for hierarchical config merge.
        let credstore = ctx
            .client_hub()
            .try_get::<dyn credstore_sdk::CredStoreClientV1>();
        let tenant_resolver = ctx
            .client_hub()
            .try_get::<dyn tenant_resolver_sdk::TenantResolverClient>();
        if credstore.is_some() || tenant_resolver.is_some() {
            state.attach_dependencies(credstore, tenant_resolver);
        }

        let state = Arc::new(state);
        self.state
            .set(state)
            .map_err(|_| anyhow::anyhow!("oagw gear already initialized"))?;
        Ok(())
    }
}

#[async_trait]
impl SystemCapability for OagwGear {
    async fn post_init(
        &self,
        _sys: &toolkit::runtime::SystemContext,
    ) -> anyhow::Result<()> {
        let _state = self
            .state
            .get()
            .ok_or_else(|| anyhow::anyhow!("oagw state not initialized"))?;
        tracing::debug!("oagw post_init: control plane ready");
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
        let state = self
            .state
            .get()
            .ok_or_else(|| anyhow::anyhow!("oagw state not initialized"))?
            .clone();
        tracing::info!("Registering oagw REST routes");
        Ok(crate::api::register_routes(router, openapi, state))
    }
}
