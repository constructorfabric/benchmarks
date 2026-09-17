//! `oagw` — the Constructor Fabric outbound API gateway.
//!
//! The gear owns two planes:
//!
//! * the **management plane** — CRUD for upstreams, their endpoint pools, the
//!   routes that select them and the plugins bound to either, mounted under
//!   `/oagw/v1/...`;
//! * the **proxy data plane** — request forwarding to the matched upstream,
//!   mounted under `/oagw/v1/proxy/{*proxy_path}`.
//!
//! ## Layout (DDD-Light)
//!
//! ```text
//! domain/    alias derivation, the model, the query language, the
//!            `ConfigStore` port, the `ControlPlane` service and the plugin SPI
//! infra/     the in-memory `ConfigStore`, the built-in plugins and the
//!            proxy data plane
//! api/       axum handlers, wire DTOs, problem rendering
//! ```
//!
//! The domain layer knows nothing about `axum` or about the storage
//! implementation, and the storage in this configuration is purely
//! in-memory (the gear has no database dependency in its `Cargo.toml`).
//!
//! ## Error contract
//!
//! Every gateway error is an RFC 9457 problem document carrying the GTS `type`
//! identifiers of `docs/DESIGN.md` §3.3 "Error Response Format" and the
//! `X-OAGW-Error-Source` header of `docs/ADR/0007`, which says whether the
//! gateway decided (`gateway`) or the upstream answered (`upstream`).
#![forbid(unsafe_code)]
#![deny(rust_2018_idioms)]

// === GEAR DEFINITION ===
pub mod gear;
pub use gear::OagwGear;

// === CONFIGURATION ===
pub mod config;
pub use config::OagwConfig;

// === INTERNAL MODULES ===
#[doc(hidden)]
pub mod api;
#[doc(hidden)]
pub mod domain;
#[doc(hidden)]
pub mod infra;

// === PUBLIC SERVICE SURFACE ===
pub use crate::domain::error::{DomainError, PluginReferences};
pub use crate::domain::services::{
    BUILT_IN_PLUGINS, BuiltInPlugin, ControlPlane, ListLimits, PluginSpec, ResolvedPlugin,
    RouteSpec, UpstreamSpec, built_in_plugin,
};
pub use crate::infra::memory::MemoryStore;
pub use crate::infra::plugin::PluginEngine;
pub use crate::infra::proxy::DataPlane;
