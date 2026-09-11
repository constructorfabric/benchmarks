//! Gear registration and lifecycle for the OAGW gear.
//!
//! Realizes `cpt-cf-oagw-flow-gf-startup`, `cpt-cf-oagw-dod-gf-registration`
//! and `cpt-cf-oagw-state-gf-lifecycle`.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use toolkit::api::OpenApiRegistry;
use toolkit::{Gear, GearCtx, RestApiCapability};
use tracing::info;

use crate::config::OagwConfig;
use crate::domain::store::Store;

/// State shared by the control plane and the data plane.
#[derive(Debug)]
pub struct OagwState {
    /// Resolved gear configuration.
    pub config: OagwConfig,
    /// The in-memory control-plane store.
    pub store: Arc<Store>,
    /// Per-instance rate-limit buckets.
    pub limiter: crate::domain::ratelimit::Limiter,
    /// Round-robin cursor over multi-endpoint pools.
    pub round_robin: crate::domain::routing::RoundRobin,
}

impl OagwState {
    /// Build state from a resolved configuration.
    #[must_use]
    pub fn new(config: OagwConfig) -> Self {
        Self {
            config,
            store: Arc::new(Store::new()),
            limiter: crate::domain::ratelimit::Limiter::new(),
            round_robin: crate::domain::routing::RoundRobin::default(),
        }
    }
}

// @cpt-begin:cpt-cf-oagw-dod-gf-registration:p1:inst-full
/// The outbound API gateway gear.
#[toolkit::gear(name = "oagw", capabilities = [rest])]
pub struct OagwGear {
    state: OnceLock<Arc<OagwState>>,
}
// @cpt-end:cpt-cf-oagw-dod-gf-registration:p1:inst-full

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            state: OnceLock::new(),
        }
    }
}

#[async_trait]
impl Gear for OagwGear {
    #[tracing::instrument(skip_all, fields(module = "oagw"))]
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        // @cpt-begin:cpt-cf-oagw-dod-gf-config:p1:inst-full
        // An absent `gears.oagw.config` section yields the documented defaults;
        // a key this build does not recognize is ignored rather than fatal.
        let config: OagwConfig = ctx.config_or_default()?;
        // @cpt-end:cpt-cf-oagw-dod-gf-config:p1:inst-full
        info!(
            proxy_timeout_secs = config.proxy_timeout_secs,
            allow_http_upstream = config.allow_http_upstream,
            ssrf_policy_enabled = config.ssrf_policy.enabled,
            token_cache_ttl_secs = config.token_cache_ttl_secs,
            token_cache_capacity = config.token_cache_capacity,
            "initializing oagw gear"
        );

        // @cpt-begin:cpt-cf-oagw-dod-gf-shared-state:p1:inst-full
        let state = Arc::new(OagwState::new(config));
        self.state
            .set(state)
            .map_err(|_| anyhow::anyhow!("oagw gear already initialized"))?;
        // @cpt-end:cpt-cf-oagw-dod-gf-shared-state:p1:inst-full

        info!("oagw gear initialized");
        Ok(())
    }
}

/// The gear's readiness check: healthy once initialization has completed and
/// the shared state the request paths depend on exists.
// @cpt-begin:cpt-cf-oagw-dod-gf-readiness:p1:inst-full
#[derive(Debug)]
struct OagwReadiness {
    ready: bool,
}

#[async_trait]
impl toolkit::Healthcheck for OagwReadiness {
    fn name(&self) -> &'static str {
        "oagw-state"
    }

    async fn check(&self) -> toolkit::HealthcheckResult {
        if self.ready {
            toolkit::HealthcheckResult::healthy()
        } else {
            toolkit::HealthcheckResult::unhealthy(
                "oagw shared state has not been initialized",
            )
        }
    }
}
// @cpt-end:cpt-cf-oagw-dod-gf-readiness:p1:inst-full

impl RestApiCapability for OagwGear {
    fn healthcheck(
        &self,
        _ctx: &GearCtx,
    ) -> Option<Arc<dyn toolkit::Healthcheck>> {
        Some(Arc::new(OagwReadiness {
            ready: self.state.get().is_some(),
        }))
    }

    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        let state = self
            .state
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("oagw state not initialized"))?;
        info!("registering oagw REST routes under /oagw/v1");
        Ok(crate::api::rest::register_routes(router, openapi, state))
    }
}
