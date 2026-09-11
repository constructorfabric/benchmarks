// @cpt-begin:cpt-cf-oagw-dod-gear-foundation-registration:p1:inst-gear
//! Gear registration and wiring.

use crate::api::rest::routes::register_routes;
use crate::api::rest::state::OagwState;
use crate::config::OagwConfig;
use async_trait::async_trait;
use axum::Router;
use credstore_sdk::CredStoreClientV1;
use std::sync::{Arc, OnceLock};
use toolkit::api::OpenApiRegistry;
use toolkit::{Gear, GearCtx, RestApiCapability};
use toolkit_http::HttpClient;
use tracing::{debug, info};

/// The outbound API gateway gear.
#[toolkit::gear(name = "oagw", capabilities = [rest])]
pub struct Oagw {
    /// Shared handler state, set once during initialisation.
    state: OnceLock<Arc<OagwState>>,
}

impl Default for Oagw {
    fn default() -> Self {
        Self {
            state: OnceLock::new(),
        }
    }
}

#[async_trait]
impl Gear for Oagw {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let config: OagwConfig = ctx.config_or_default()?;
        debug!(
            proxy_timeout_secs = config.proxy_timeout_secs,
            allow_http_upstream = config.allow_http_upstream,
            ssrf_enforced = config.ssrf_policy.enabled,
            "loaded oagw configuration"
        );

        // The outbound client must be able to dial plaintext when the gear
        // configuration permits it; whether it does is decided per request.
        let http = HttpClient::builder()
            .with_otel()
            .build()
            .map_err(|e| anyhow::anyhow!("failed to build the outbound HTTP client: {e}"))?;

        // The credential store is optional: an upstream that injects no
        // credential does not need it.
        let cred_store: Option<Arc<dyn CredStoreClientV1>> =
            ctx.client_hub().get::<dyn CredStoreClientV1>().ok();
        if cred_store.is_none() {
            debug!("no credential store is registered; secret references will not resolve");
        }

        let state = Arc::new(OagwState::new(config, http, cred_store));
        if self.state.set(state).is_err() {
            anyhow::bail!("oagw state was already initialised");
        }
        info!("oagw gear initialised");
        Ok(())
    }
}

impl RestApiCapability for Oagw {
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<Router> {
        let state = self
            .state
            .get()
            .ok_or_else(|| anyhow::anyhow!("oagw state is not initialised"))?
            .clone();
        Ok(register_routes(router, openapi, state))
    }
}
// @cpt-end:cpt-cf-oagw-dod-gear-foundation-registration:p1:inst-gear
