// Created: 2026-08-29 by Constructor Tech
//! `oagw` — outbound API gateway.
//!
//! The crate is split along the usual seams:
//!
//! - [`config`] — gear configuration.
//! - [`domain`] — wire model, alias derivation, hierarchical merge, plugin
//!   traits and the control-plane service (transport free).
//! - [`infra`] — in-memory storage, plugin implementations, the proxy engine,
//!   rate limiting and the circuit breaker.
//! - [`api`] — axum DTOs, wire errors, handlers and routes.
//! - [`gear`] — toolkit gear registration.

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;

pub use config::OagwConfig;
pub use domain::error::OagwError;
pub use domain::model::{PluginCreate, RouteCreate, UpstreamCreate};
pub use gear::Oagw;

/// GTS type ids used by the gear.
pub mod types {
    /// Upstream entity type id.
    pub const UPSTREAM: &str = "gts.cf.core.oagw.upstream.v1";
    /// Route entity type id.
    pub const ROUTE: &str = "gts.cf.core.oagw.route.v1";
    /// Plugin entity type id.
    pub const PLUGIN: &str = "gts.cf.core.oagw.plugin.v1";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_are_reachable_through_the_crate_root() {
        assert_eq!(OagwConfig::default().proxy_timeout_secs, 30);
    }

    #[test]
    fn crate_error_type_round_trips() {
        let error = OagwError::RouteNotFound("nope".to_owned());
        assert_eq!(error.status(), 404);
    }
}
