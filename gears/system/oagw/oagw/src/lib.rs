//! # OAGW — Outbound API Gateway
//!
//! OAGW is the platform's egress gateway: application gears reach external
//! services through it instead of holding credentials and connection details
//! themselves. It is a single ToolKit gear with an internal Control
//! Plane / Data Plane split (see `docs/DESIGN.md`):
//!
//! * **Control Plane** ([`domain::services`]) owns configuration —
//!   upstreams, routes and custom plugin definitions — and answers alias
//!   resolution queries across the tenant hierarchy.
//! * **Data Plane** ([`infra::proxy`]) executes proxy requests: it resolves the
//!   effective configuration, runs the plugin chain (auth → guards →
//!   transforms), and forwards the call to the external service, streaming
//!   plain HTTP responses, server-sent events and WebSocket upgrades alike.
//!
//! Routes are registered **gear-relative** (`/oagw/v1/...`); the api-gateway
//! nests the gear router under its own `prefix_path`.
#![forbid(unsafe_code)]

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;

#[cfg(feature = "test-utils")]
pub mod test_utils;


pub use config::OagwConfig;
pub use gear::OagwGear;

