// Created: 2026-09-04 by Constructor Tech
//! Outbound API Gateway (OAGW) gear.
//!
//! OAGW is the centralized outbound API gateway of Constructor Fabric: it
//! manages tenant-scoped upstream definitions, matches inbound proxy requests
//! against routes, and forwards them to external services while applying
//! authentication, rate limiting, CORS and header policies
//! (`docs/DESIGN.md` §1.1).
//!
//! The crate follows the DDD-Light layering of the design:
//!
//! * `domain/` — value objects, invariants and pure resolution logic; no
//!   infrastructure dependency.
//! * `error` — the domain error model and its RFC 9457
//!   (`application/problem+json`) projection.
//! * `config` — the gear-level configuration read from `gears.oagw.config`.
//! * `controlplane/` — the in-memory, tenant-scoped source of truth and the
//!   service enforcing every management invariant.
//! * `api/` — the `/oagw/v1` management REST surface.
//! * `dataplane/` — the proxy engine: resolution, plugin chain, rate limiting,
//!   CORS, error semantics and streaming (SSE and WebSocket upgrades).
//! * `gear` — the `#[toolkit::gear]` declaration, its `Gear::init` bootstrap
//!   and the gear-relative REST registration.
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
// The coverage workflow enables the nightly `coverage` attribute through
// `RUSTFLAGS="--cfg coverage_nightly"`. This crate does not inherit the
// workspace lint table (`[lints] workspace = true` is absent from its
// manifest), so `coverage_nightly` has to be registered here to keep the
// build warning-free under `-D warnings`.
#![allow(unexpected_cfgs)]

// === GEAR CONFIGURATION ===
pub mod config;

// === GEAR WIRING ===
pub mod gear;
pub use gear::OagwGear;

// === INTERNAL MODULES ===
pub mod api;
#[doc(hidden)]
pub mod controlplane;
#[doc(hidden)]
pub mod dataplane;
#[doc(hidden)]
pub mod domain;
#[doc(hidden)]
pub mod error;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
pub(crate) mod test_support;
