//! `oagw` — the Outbound API Gateway.
//!
//! OAGW is the single egress point for every outbound API call the platform
//! makes. Application gears do not hold credentials, connection details or
//! policy for external services: they call OAGW's proxy endpoint with an
//! alias, and OAGW resolves the upstream, injects the credential, applies the
//! configured policy and forwards the request.
//!
//! # Layout
//!
//! The crate follows the DDD-light layering of
//! `gears/system/oagw/docs/DESIGN.md`:
//!
//! * [`domain`] — models, service traits, repository contracts and the plugin
//!   traits. No dependency on transport or infrastructure.
//! * [`infra`] — repository implementations, the Pingora-backed proxy engine,
//!   plugin registries, rate limiters, metrics and GTS provisioning.
//! * [`api`] — the REST transport: DTOs, handlers and route registration.
//!
//! # The two planes
//!
//! * **Control Plane** ([`domain::services::ControlPlane`]) owns configuration:
//!   CRUD for upstreams, routes and plugins, alias resolution, and the
//!   effective-configuration merge across the tenant hierarchy.
//! * **Data Plane** ([`infra::proxy::DataPlane`]) owns request execution:
//!   endpoint selection, the auth/guard/transform chain, rate limiting, and
//!   forwarding — plain HTTP, server-sent-event streams and WebSocket
//!   upgrades alike.
//!
//! # Route paths
//!
//! Routes are registered **gear-relative** (`/oagw/v1/...`). The API gateway
//! nests each gear's router under its own `prefix_path`, so the absolute path
//! a deployment serves is `{prefix_path}/oagw/v1/...`.

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;

pub use config::OagwConfig;
pub use gear::OagwGear;

#[cfg(test)]
mod integration_tests;
