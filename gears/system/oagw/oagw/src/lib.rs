//! # OAGW — the Outbound API Gateway
//!
//! Every outbound call a gear makes to an external service goes through this
//! gear, so credentials, rate limits, header hygiene and SSRF policy live in
//! one place instead of being reimplemented per caller.
//!
//! ## Shape
//!
//! A Control Plane / Data Plane split inside a single crate, layered
//! `api` → `domain` → `infra` (ADR-0001):
//!
//! * [`domain::services::ControlPlaneService`] owns the configuration
//!   objects — upstreams, routes and custom plugins — and every CRUD
//!   operation on them is scoped to the calling tenant.
//! * [`infra::proxy::DataPlaneService`] runs one proxy request: resolve the
//!   alias across the tenant hierarchy, match a route, merge the effective
//!   configuration, run `Auth → Guards → Transform`, forward, and stream the
//!   answer back.
//!
//! ## Wire surface
//!
//! Routes are registered **gear-relative** under `/oagw/v1/...`; the API
//! gateway nests this router under its own `prefix_path`, so a deployment
//! with `prefix_path: /api` serves them at `/api/oagw/v1/...`.
//!
//! | Path | Purpose |
//! |---|---|
//! | `/oagw/v1/upstreams` | Upstream CRUD |
//! | `/oagw/v1/routes` | Route CRUD |
//! | `/oagw/v1/plugins` | Custom plugin CRUD |
//! | `/oagw/v1/proxy/{alias}[/{path}]` | Proxy |
//!
//! Gateway-originated errors are RFC 9457 Problem Details carrying a GTS
//! `type` identifier and `X-OAGW-Error-Source: gateway`; anything that came
//! from the upstream is passed through unchanged and tagged `upstream`.

// `OagwError` is deliberately wide: it *is* the Problem Details document,
// carrying the title, detail, extension members and response headers the
// wire contract specifies. Boxing it would buy a smaller `Result` at the cost
// of an allocation on every validation failure, on paths that are already
// returning to the network.
#![allow(clippy::result_large_err)]

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;

#[cfg(any(test, feature = "test-utils"))]
pub mod test_support;

pub use config::OagwConfig;
pub use domain::error::{OagwError, PluginError};
pub use domain::plugin::{
    AuthPlugin, GuardDecision, GuardPlugin, RequestContext, ResponseContext, TransformPlugin,
};
pub use domain::services::ControlPlaneService;
pub use gear::OagwGear;
pub use infra::proxy::DataPlaneService;
