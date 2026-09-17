//! Outbound API Gateway (OAGW) gear.
//!
//! OAGW is the single egress point for tenant applications calling external
//! HTTP services. It is split into two halves inside one crate:
//!
//! * **Control Plane** (`domain::services::management`, `infra::storage`) owns
//!   upstream/route/plugin configuration: validation, tenant scoping, CRUD.
//! * **Data Plane** (`domain::services::proxy`, `infra::proxy`) executes proxy
//!   requests: alias resolution, route matching, plugin chain, rate limiting,
//!   header transformation and the upstream call itself (HTTP, SSE, WebSocket).
//!
//! Both halves are exposed through one REST surface registered gear-relative
//! under `/oagw/v1` (the api-gateway applies its own prefix, which is empty in
//! the e2e deployment).
//!
//! * [PRD](../../docs/PRD.md)
//! * [Design](../../docs/DESIGN.md)
//! * [ADR: request routing](../../docs/ADR/0001-request-routing.md)

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;

pub use config::OagwConfig;
pub use gear::OagwGear;
