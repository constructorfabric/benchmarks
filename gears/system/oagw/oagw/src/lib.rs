//! `oagw` — the Constructor Fabric outbound API gateway gear.
//!
//! The crate is split the way DESIGN §3 lays it out:
//!
//! - [`domain`] — pure business rules: DTOs, validation, matching, layering,
//!   rate-limit arithmetic, plugin contracts and the control/data-plane
//!   services. Nothing here touches a transport.
//! - [`infra`] — adapters: in-memory repositories, the plugin registry, the
//!   upstream connector, the circuit breaker, metrics and the WebSocket
//!   handshake helpers.
//! - [`api`] — the axum handlers that translate HTTP into domain calls.
//! - [`gear`] — the `#[toolkit::gear]` declaration that wires all of it into
//!   the host runtime.

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;

pub use config::OagwConfig;
pub use domain::error::{DomainError, ErrorKind};
pub use gear::OagwGear;

#[cfg(test)]
mod config_tests;
