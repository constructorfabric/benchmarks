//! Outbound API Gateway (OAGW) gear.
//!
//! OAGW discovers and routes outbound traffic to upstream services:
//!
//! * **Control plane** — tenant-scoped CRUD for upstreams, routes, and custom
//!   plugins (`/oagw/v1/upstreams`, `/oagw/v1/routes`, `/oagw/v1/plugins`)
//!   with hierarchical config merge across the tenant chain.
//! * **Data plane** — `/oagw/v1/proxy/{alias}/{*rest}` forwards requests with
//!   auth injection (`noop` / `apikey` / `oauth2_client_cred[_basic]`),
//!   guard + transform plugins, rate limiting, CORS, and header
//!   transformation (see [`crate::proxy`]).
//!
//! Specification: `gears/system/oagw/docs/` (PRD.md, DESIGN.md, ADR/,
//! schemas/).

pub mod alias;
pub mod api;
pub mod config;
pub mod error;
pub mod gts;
pub mod model;
mod gear;
pub mod proxy;
pub mod state;
pub mod tenant;

pub use gear::OagwGear;
