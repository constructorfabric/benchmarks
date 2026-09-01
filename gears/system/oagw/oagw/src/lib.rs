//! Outbound API Gateway (OAGW) gear.
//!
//! OAGW is the single egress point for all outbound HTTP(S) traffic from Gears
//! to external services. The crate is split into three layers following
//! DDD-Light:
//!
//! * [`domain`] — business logic: the upstream/route/plugin model, alias
//!   derivation, hierarchical configuration merge, route matching, rate-limit
//!   and CORS policy, and the [`ControlPlaneService`] / [`DataPlaneService`]
//!   traits. No infrastructure dependencies.
//! * [`infra`] — in-memory repositories, the outbound HTTP client (with TLS
//!   via `pingora`'s rustls connector), the built-in plugin implementations,
//!   the rate-limiter registry and the OpenTelemetry metric instruments.
//! * [`api`] — axum transport layer: DTOs, extractors, error mapping and
//!   route registration.
//!
//! The Control Plane owns configuration data (upstreams, routes, plugins);
//! the Data Plane resolves an effective configuration for every proxy request
//! and executes the plugin chain before and after the upstream call.
#![forbid(unsafe_code)]

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;

pub use config::OagwConfig;
