//! # OAGW — the outbound API gateway gear
//!
//! OAGW registers upstream services and routing rules, then proxies outbound
//! traffic to them: plain HTTP exchanges, server-sent-event streams and
//! WebSocket upgrades alike.
//!
//! The gear serves two surfaces, both mounted gear-relative under `/oagw/v1`:
//!
//! * a management API (`/oagw/v1/upstreams`, `/oagw/v1/routes`,
//!   `/oagw/v1/plugins`) that is the control plane, and
//! * a proxy API (`/oagw/v1/proxy/{alias}/{path}`) that is the data plane.
//!
//! Paths are registered gear-relative on purpose. The api-gateway host merges
//! every gear's routes onto one shared router and nests the whole assembled
//! router once under its own `prefix_path`, so a gear must not repeat that
//! prefix itself.
//!
//! The implementation contract lives in `gears/system/oagw/docs/`: `PRD.md`,
//! `DESIGN.md`, the accepted ADRs, and the seven FEATURE documents under
//! `docs/features/`. Code carries `@cpt-begin` / `@cpt-end` markers tracing
//! back to those documents' CDSL identifiers.

#![allow(clippy::multiple_crate_versions)]

pub mod config;
pub mod gear;

#[doc(hidden)]
pub mod api;
pub mod domain;
#[doc(hidden)]
pub mod infra;

pub use config::{OagwConfig, SsrfPolicy};
pub use gear::{OagwGear, OagwState};
