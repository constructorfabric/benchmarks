//! OAGW — outbound API gateway gear.
//!
//! The gear exposes two surfaces:
//!
//! * a **management API** under `/oagw/v1/{upstreams,routes,plugins}` for
//!   CRUD over tenant-scoped upstreams, routes and plugins, and
//! * a **proxy API** under `/oagw/v1/proxy/{alias}/{*path}` that resolves an
//!   upstream by alias, walks the tenant chain, runs the plugin chain and
//!   forwards the request to the upstream, streaming the response back.
//!
//! Gateway-originated failures are rendered as RFC 9457
//! `application/problem+json` with the GTS `type` identifiers tabulated in
//! DESIGN §3.3 and carry `X-OAGW-Error-Source: gateway`; upstream failures are
//! passed through and stamped `X-OAGW-Error-Source: upstream`.

// The `coverage_nightly` cfg is declared by the workspace `[lints]` table; this
// crate does not inherit that table, so the declaration has to live here.
#![allow(unexpected_cfgs)]

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;
pub mod proxy;

pub use config::OagwConfig;
pub use domain::error::{DomainError, ErrorKind};
pub use domain::model::{Route, Upstream};
pub use gear::Oagw;
pub use infra::state::GearState;
