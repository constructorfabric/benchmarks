//! # OAGW — Outbound API Gateway
//!
//! The `oagw` gear is Constructor Fabric's outbound API gateway: a control plane
//! that stores upstream/route/plugin configuration, and a data plane that
//! proxies requests to declared upstream services over HTTP, SSE and
//! WebSocket.
//!
//! Layout:
//!
//! * [`config`] — gear configuration (`oagw.config` in the server YAML)
//! * [`gts`] — GTS identifiers cataloged for the types-registry
//! * [`domain`] — the configuration model, validation, alias derivation and the
//!   in-memory store that backs the management API
//! * [`plugins`] — the three plugin traits (auth / guard / transform) plus the
//!   built-in implementations
//! * [`proxy`] — the data plane
//! * [`api`] — the management REST API
//! * [`gear`] — gear registration

// === MODULE DEFINITION ===
pub mod gear;
pub use gear::OagwGear;

// === INTERNAL MODULES ===
pub mod api;
pub mod config;
pub mod domain;
pub mod gts;
pub mod plugins;
pub mod proxy;

/// Crate-local result alias.
pub type OagwResult<T> = Result<T, crate::domain::error::OagwError>;
