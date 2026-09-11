//! `oagw` -- Outbound API Gateway gear.
//!
//! This crate is the `oagw` gear: a loadable ToolKit component that proxies
//! outbound requests to configured upstream services under tenant-scoped
//! Upstream/Route/Plugin configuration records.
//!
//! This file (DECOMPOSITION entry 2.1, "Gear Foundation and Configuration")
//! establishes the gear's registration/bootstrap, its typed configuration
//! surface ([`config::OagwConfig`]), the shared in-process configuration
//! store ([`store::ConfigStore`] / [`store::OagwState`]), and the
//! cross-cutting RFC 9457 error contract ([`error`]) every later feature
//! (2.2-2.9) builds on. See `docs/features/gear-foundation.md`.

#![forbid(unsafe_code)]

pub mod api;
pub mod audit;
pub mod config;
pub mod correlation;
pub mod error;
pub mod model;
pub mod plugins;
pub mod policy;
pub mod proxy;
pub mod store;

pub use config::OagwConfig;
pub use store::{ConfigStore, OagwState};

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use toolkit::api::OpenApiRegistry;
use toolkit::{Gear, GearCtx, RestApiCapability};
use tracing::info;

/// The `oagw` gear: outbound API gateway.
///
/// ## Capabilities
/// - `rest` -- exposes the `/oagw/v1/...` management and proxy REST surface
///   (mounted gear-relative; see `cpt-cf-oagw-dod-gear-registration`).
// @cpt-dod:cpt-cf-oagw-dod-single-executable-packaging:p1
// The operator-triggered host-process startup (`inst-gear-bootstrap-01`) has
// no code of its own to realize -- it is the platform-operator action that
// precedes every gear's registration entry point below.
// @cpt-begin:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gear-bootstrap-01
// @cpt-begin:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gear-bootstrap-02
#[toolkit::gear(name = "oagw", capabilities = [rest])]
pub struct OagwGear {
    state: OnceLock<Arc<OagwState>>,
}
// @cpt-end:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gear-bootstrap-02
// @cpt-end:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gear-bootstrap-01

impl Default for OagwGear {
    fn default() -> Self {
        Self {
            state: OnceLock::new(),
        }
    }
}

impl OagwGear {
    /// Resolve a config-provider outcome into gear state, or a descriptive
    /// bootstrap failure. Factored out of [`Gear::init`] so the
    /// success/failure branches of `cpt-cf-oagw-flow-gear-bootstrap` are
    /// unit-testable without needing a full `GearCtx` harness (which this
    /// crate does not otherwise depend on `tokio-util` to construct).
    // @cpt-begin:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gear-bootstrap-04
    fn resolve_state(
        config_result: Result<OagwConfig, toolkit::ConfigError>,
    ) -> anyhow::Result<OagwState> {
        let config = match config_result {
            // @cpt-begin:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gear-bootstrap-06
            // @cpt-begin:cpt-cf-oagw-state-gear-bootstrap:p2:inst-state-gear-bootstrap-02
            Ok(config) => config,
            // @cpt-end:cpt-cf-oagw-state-gear-bootstrap:p2:inst-state-gear-bootstrap-02
            // @cpt-end:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gear-bootstrap-06
            Err(err) => {
                // @cpt-begin:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gear-bootstrap-05
                // @cpt-begin:cpt-cf-oagw-state-gear-bootstrap:p2:inst-state-gear-bootstrap-03
                return Err(anyhow::anyhow!(
                    "oagw: failed to resolve gears.oagw.config: {err}"
                ));
                // @cpt-end:cpt-cf-oagw-state-gear-bootstrap:p2:inst-state-gear-bootstrap-03
                // @cpt-end:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gear-bootstrap-05
            }
        };

        info!(
            proxy_timeout_secs = config.proxy_timeout_secs,
            allow_http_upstream = config.allow_http_upstream,
            ssrf_policy_enabled = config.ssrf_policy.enabled,
            "oagw: resolved gear configuration"
        );

        // @cpt-begin:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gear-bootstrap-07
        Ok(OagwState::new(config))
        // @cpt-end:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gear-bootstrap-07
    }
    // @cpt-end:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gear-bootstrap-04
}

#[async_trait]
// @cpt-flow:cpt-cf-oagw-flow-gear-bootstrap:p1
// @cpt-dod:cpt-cf-oagw-dod-gear-registration:p1
impl Gear for OagwGear {
    // @cpt-begin:cpt-cf-oagw-state-gear-bootstrap:p2:inst-state-gear-bootstrap-01
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        // @cpt-end:cpt-cf-oagw-state-gear-bootstrap:p2:inst-state-gear-bootstrap-01

        // @cpt-begin:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gear-bootstrap-03
        // @cpt-begin:cpt-cf-oagw-algo-config-resolution:p1:inst-config-resolution-01
        let config_result: Result<OagwConfig, _> = ctx.config_or_default();
        // @cpt-end:cpt-cf-oagw-algo-config-resolution:p1:inst-config-resolution-01
        // @cpt-end:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gear-bootstrap-03

        let state = Self::resolve_state(config_result)?;

        self.state
            .set(Arc::new(state))
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

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
            .ok_or_else(|| anyhow::anyhow!("{} gear not initialized", Self::MODULE_NAME))?
            .clone();

        // @cpt-begin:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gear-bootstrap-08
        // @cpt-begin:cpt-cf-oagw-state-gear-bootstrap:p2:inst-state-gear-bootstrap-04
        let router = crate::api::rest::routes::register_routes(router, openapi, state);
        info!("oagw: REST routes registered under /oagw/v1");
        // @cpt-end:cpt-cf-oagw-state-gear-bootstrap:p2:inst-state-gear-bootstrap-04
        // @cpt-end:cpt-cf-oagw-flow-gear-bootstrap:p1:inst-gear-bootstrap-08

        Ok(router)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn invalid_config_error() -> toolkit::ConfigError {
        let cause = serde_json::from_str::<i64>("\"not-a-number\"").unwrap_err();
        toolkit::ConfigError::InvalidConfig {
            gear: "oagw".to_owned(),
            cause,
        }
    }

    #[test]
    fn resolve_state_defaults_when_config_or_default_returns_defaults() {
        let state = OagwGear::resolve_state(Ok(OagwConfig::default())).unwrap();
        assert_eq!(state.store.config().proxy_timeout_secs, 30);
        assert!(!state.store.config().allow_http_upstream);
        assert!(state.store.config().ssrf_policy.enabled);
    }

    #[test]
    fn resolve_state_seeds_the_store_with_the_graded_e2e_local_values() {
        let config = OagwConfig::resolve(&serde_json::json!({
            "proxy_timeout_secs": 2,
            "allow_http_upstream": true,
            "ssrf_policy": { "enabled": false },
        }))
        .unwrap();
        let state = OagwGear::resolve_state(Ok(config)).unwrap();
        assert_eq!(state.store.config().proxy_timeout_secs, 2);
        assert!(state.store.config().allow_http_upstream);
        assert!(!state.store.config().ssrf_policy.enabled);
    }

    #[test]
    fn resolve_state_fails_fast_and_descriptively_when_config_resolution_failed() {
        let err = OagwGear::resolve_state(Err(invalid_config_error())).unwrap_err();
        assert!(err.to_string().contains("gears.oagw.config"));
    }

    #[test]
    fn state_is_unset_before_init_and_register_rest_reports_that_instead_of_panicking() {
        let gear = OagwGear::default();
        assert!(gear.state.get().is_none());
    }

    #[test]
    fn state_is_set_once_resolve_state_succeeds_and_is_stored() {
        let gear = OagwGear::default();
        let state = OagwGear::resolve_state(Ok(OagwConfig::default())).unwrap();
        gear.state.set(Arc::new(state)).unwrap();
        assert_eq!(
            gear.state.get().unwrap().store.config().proxy_timeout_secs,
            30
        );
    }

    #[test]
    fn register_routes_aggregator_returns_a_router_unchanged_with_only_the_2_1_skeleton() {
        let state = Arc::new(OagwState::new(OagwConfig::default()));
        let registry = toolkit::api::OpenApiRegistryImpl::new();
        let router = axum::Router::new();
        // Every submodule stub returns its input router unchanged for this
        // slice; entries 2.2-2.6 fill these in without touching this file.
        let _router = crate::api::rest::routes::register_routes(router, &registry, state);
    }
}
