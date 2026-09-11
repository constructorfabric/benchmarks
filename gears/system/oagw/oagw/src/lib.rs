//! `oagw` — the outbound API gateway gear.
//!
//! Three layers, per DESIGN §3.2:
//!
//! * [`api::rest`] — the management REST surface and the proxy entry point.
//! * [`domain`] — the model, its validation rules, the control-plane service
//!   and the plugin contracts.
//! * [`infra`] — the in-memory store, the built-in plugins and the data plane
//!   that talks to upstreams.
//!
//! Routing (`X-OAGW-Target-Host`), rate limiting (ADR 0003), CORS (ADR 0004)
//! and error-source marking (ADR 0007) all live in [`infra::proxy`].

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;

pub use gear::OagwGear;
