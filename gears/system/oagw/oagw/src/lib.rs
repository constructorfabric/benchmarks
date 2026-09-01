//! OAGW — outbound API gateway gear.
//!
//! OAGW discovers and routes to upstream services (ADR-0001): tenants manage
//! upstreams, routes and plugins through a REST control plane; the proxy
//! endpoint resolves an upstream by alias across the tenant hierarchy, applies
//! auth / guard / transform plugins, rate limits, header policies, CORS and
//! SSRF protection, then forwards the request with a bounded timeout.
//!
//! Persistence is in-memory (this build has no SQL backend); custom Starlark
//! plugins are stored and validated but not executed (binding one fails
//! closed with `503 PluginNotFound`).

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

/// Crate API surface (management + proxy handlers).
pub mod api;
/// Gear configuration (`OagwConfig`).
pub mod config;
/// Domain model, services, plugin contracts and error types.
pub mod domain;
/// The gear definition and wiring (toolkit `#[gear]`).
pub mod gear;
/// GTS identifier constants.
pub mod gts;
/// Implementation details: memory persistence, data plane, plugins, CORS,
/// SSRF, aliasing and rate limiting.
pub mod infra;

pub use config::OagwConfig;
pub use gear::OagwGear;
