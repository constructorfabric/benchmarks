// Updated: 2026-09-01 by Constructor Tech
//! The `oagw` gear — Constructor Fabric's outbound API gateway.
//!
//! Three layers, per DESIGN §3.2:
//!
//! * [`domain`] — the models the REST layer deserializes, the storage layer
//!   persists and the Data Plane consumes, plus the management service that
//!   owns every tenancy and authorisation rule.
//! * [`api`] — the axum handlers, one per route in the OpenAPI document.
//! * [`infra`] — what the domain talks to but never knows about: the in-memory
//!   store, the builtin plugin implementations, and the proxy engine.
//!
//! [`gear`] is the seam that registers all of it with the host runtime.

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod gts;
pub mod infra;

/// The name the gear registers itself under.
pub const GEAR_NAME: &str = "oagw";

#[cfg(test)]
mod tests {
    #[test]
    fn module_tree_is_reachable() {
        assert_eq!(crate::GEAR_NAME, "oagw");
        assert_eq!(crate::config::OagwConfig::default().api_prefix, "/oagw/v1");
    }
}
