//! OAGW — the outbound API gateway gear.
//!
//! The crate is split into the domain layer (models, validation, errors and
//! the control-plane service), the infrastructure layer (persistence, the
//! outbound HTTP client, the plugin registry and the proxy data plane) and
//! the REST surface that wires both together.

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;

pub use gear::OagwGear;
