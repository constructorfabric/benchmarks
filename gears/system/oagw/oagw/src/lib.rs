//! OAGW — the outbound API gateway.
//!
//! The crate inherits no workspace lint table (its `Cargo.toml` has no
//! `[lints]` section), so the `coverage_nightly` check-cfg used by the
//! workspace's coverage tooling is silenced here rather than declared.
//!
#![allow(unexpected_cfgs)]
//! The gear exposes a tenant-scoped control plane (`/oagw/v1/upstreams`,
//! `/oagw/v1/routes`, `/oagw/v1/plugins`) and a method-agnostic data plane
//! (`/oagw/v1/proxy/{alias}/...`) that resolves the matching route, runs the
//! plugin chain, and relays the request over plain HTTP, an SSE stream, or a
//! WebSocket upgrade.
//!
//! # Layers
//!
//! - `api` — transport: route registration and handlers
//! - `domain` — model, validation, routing, plugins, services
//! - `infra` — authoritative in-process store, proxy engine, plugins
//!
//! Every gateway-produced failure is an RFC 9457 `application/problem+json`
//! response carrying the crate's normative GTS `type` identifier and the
//! `X-OAGW-Error-Source: gateway` header (`ADR 0007`).

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;

#[cfg(test)]
mod tests;

/// Gear type registered with the runtime.
pub use gear::OagwGear;
