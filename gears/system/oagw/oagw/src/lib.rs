#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
//! OAGW — outbound API gateway gear.
//!
//! OAGW centralizes every outbound call a gear makes to an external service
//! behind one proxy surface. It is split into a **control plane** (management
//! CRUD for upstreams, routes and plugin bindings) and a **data plane** (the
//! `/oagw/v1/proxy/{alias}` request path), both living in this single crate
//! with DDD-Light layering:
//!
//! - `domain` — business logic and the model types shaped after
//!   `docs/schemas/upstream.v1.schema.json` / `route.v1.schema.json`; no
//!   transport or persistence dependencies.
//! - `api` — Axum transport layer: `OperationBuilder` route registration,
//!   handlers, wire DTOs.
//!
//! Slice S1 ships the crate skeleton, the configuration model, the gear
//! wiring (no database is required — the control plane is in-memory), the
//! error model and the upstream control-plane CRUD. Slice S2 completes the
//! management surface with route and plugin CRUD and adds the data plane:
//! target resolution ([`domain::resolution`]) and the streaming
//! `/oagw/v1/proxy/{alias}` pass-through ([`domain::proxy`]). Slice S4 adds the
//! policies the proxy path enforces on the way through: the plugin chain
//! ([`domain::plugins`]), the rate limiter ([`domain::rate_limit`]) and CORS
//! ([`domain::cors`]).
//!
//! # Error contract
//!
//! Every gateway-generated response carries `X-OAGW-Error-Source: gateway`
//! and errors are RFC 9457 `application/problem+json` documents whose `type`
//! is a GTS identifier under `gts.cf.core.errors.err.v1~cf.oagw.<name>.v1`
//! (see ADR-0007).

pub mod api;
pub mod domain;

mod config;
mod error;
mod gear;

pub use error::{
    ApiResult, ERROR_SOURCE_HEADER, ErrorExtensions, ErrorKind, GATEWAY_ERROR_SOURCE, OagwError,
    OagwProblem, UPSTREAM_ERROR_SOURCE,
};
pub use gear::OagwGear;
