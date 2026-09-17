//! OAGW — Outbound API Gateway Gear.
//!
//! The OAGW gear is the outbound API gateway of the gears-rust platform. It
//! provides a control plane (upstream / route / plugin CRUD) and a data plane
//! (authenticated proxying to registered upstream services) in a single crate.
//!
//! ## Architecture
//!
//! - **Control plane**: tenants register upstream services (endpoints, auth,
//!   headers, plugins, rate limits, CORS), routes (match rules binding a path
//!   prefix + method set to an upstream), and plugins via the REST API
//!   (`/oagw/v1/upstreams`, `/oagw/v1/routes`, `/oagw/v1/plugins`).
//! - **Data plane**: the proxy entry point `/oagw/v1/proxy/{alias}/{*path}`
//!   resolves the registered configuration (including hierarchical
//!   tenant-enforced limits), runs the auth → guards → transform → upstream
//!   → transform pipeline, and streams the upstream response back.
//!
//! ## Sources of truth
//!
//! The wire contract, semantics, and error model are specified in
//! `gears/system/oagw/docs/` (PRD, DESIGN, ADRs, JSON schemas). This crate is
//! the implementation of that specification.
//!
//! ## Registration
//!
//! `OagwGear` registers itself with the runtime through `#[toolkit::gear]` and
//! is discovered from the process inventory by the example server's
//! `registered_gears.rs` (`use api_egress as _;`). REST routes are served
//! gear-relative under `{prefix}/oagw/v1/...` (the api-gateway nests each gear
//! router under its configured `prefix_path`, which is empty in the e2e setup).

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms)]

// === GEAR DECLARATION ===
pub mod gear;
pub use gear::OagwGear;

// === CONFIGURATION ===
pub mod config;

// === GTS VOCABULARY ===
pub mod gts;

// === INTERNAL MODULES ===
#[doc(hidden)]
pub mod api;
#[doc(hidden)]
pub mod domain;
#[doc(hidden)]
pub mod infra;
