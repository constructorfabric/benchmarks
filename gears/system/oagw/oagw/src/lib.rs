//! `oagw` — the Outbound API Gateway.
//!
//! Manages every outbound API request the platform makes: credential
//! injection, rate limiting, header transformation and security policy, behind
//! one proxy endpoint.
//!
//! Layering follows DDD-Light (`domain` / `infra` / `api`) with a Control
//! Plane / Data Plane split inside the single crate (see `docs/DESIGN.md`).
//!
//! The registered surface is **gear-relative** — `/oagw/v1/...` — because the
//! api-gateway nests each gear's router under its own `prefix_path`.

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;
pub mod util;

pub use config::OagwConfig;
pub use domain::error::{ErrorKind, ErrorSource, OagwError};
pub use gear::OagwGear;
