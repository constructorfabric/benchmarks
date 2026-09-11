//! The proxy data plane of entry 2.4
//! (`cpt-cf-oagw-flow-request-proxy-dispatch`).
//!
//! | module | owns |
//! |---|---|
//! | [`service`] | `DataPlaneServiceImpl`, the pipeline the handler dispatches to |
//! | [`alias_resolver`] | the tenant-chain alias walk and its shadowing |
//! | [`endpoint_selector`] | the round-robin cursor state per endpoint pool |
//! | [`rate_limiter`] | the in-memory counter registry of entry 2.7 |
//! | [`body_validation`] | the declared and observed body framing checks |
//! | [`stream`] | the SSE relay and the streaming-session lifecycle |
//! | [`upgrade`] | the WebSocket relay and its bidirectional pump |
//!
//! The pure algorithms the pipeline runs live in the domain
//! ([`crate::domain::route_matcher`], [`crate::domain::headers`],
//! [`crate::domain::endpoints`]); this module owns the parts that cross a
//! boundary the domain may not name: the repositories, the tenant resolver,
//! the HTTP client and the socket handoff of an upgrade.

pub mod alias_resolver;
pub mod body_validation;
pub mod endpoint_selector;
pub mod rate_limiter;
pub mod service;
pub mod stream;
pub mod upgrade;

pub use rate_limiter::RateLimiterRegistry;
pub use service::{
    DataPlaneLimits, DataPlaneServiceImpl, HierarchyPermissions, NoHierarchyPermissions, TransportObserver,
};

#[cfg(test)]
#[path = "alias_resolver_tests.rs"]
mod alias_resolver_tests;

#[cfg(test)]
#[path = "body_validation_tests.rs"]
mod body_validation_tests;

#[cfg(test)]
#[path = "cors_gate_tests.rs"]
mod cors_gate_tests;

#[cfg(test)]
#[path = "endpoint_selector_tests.rs"]
mod endpoint_selector_tests;

#[cfg(test)]
#[path = "rate_limiter_tests.rs"]
mod rate_limiter_tests;

#[cfg(test)]
#[path = "rate_limit_gate_tests.rs"]
mod rate_limit_gate_tests;

#[cfg(test)]
#[path = "stream_lifecycle_tests.rs"]
mod stream_lifecycle_tests;

#[cfg(test)]
#[path = "upgrade_tests.rs"]
mod upgrade_tests;
