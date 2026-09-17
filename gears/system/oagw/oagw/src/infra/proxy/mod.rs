//! Data plane of the `oagw` gear (DESIGN.md §3.2, §3.5).
//!
//! [`service::DataPlaneServiceImpl`] orchestrates one proxied request: it
//! resolves the upstream and route through the control plane, enforces the
//! plaintext, body, rate limit and circuit breaker policies, runs the plugin
//! chain (ADR-0002) and dials the upstream exactly once with
//! [`toolkit_http`].

pub mod builtins;
pub mod circuit;
pub mod cors;
pub mod headers;
pub mod service;
pub mod streaming;
pub mod target;

pub use builtins::BuiltinPlugins;
pub use circuit::CircuitBreakers;
pub use service::{DataPlaneServiceImpl, ProxyOutcome, ProxyRequest};
pub use target::EndpointSelector;
