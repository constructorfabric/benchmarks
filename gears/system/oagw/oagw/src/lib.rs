// Created: 2026-09-01 by Constructor Tech
//! # OAGW — Outbound API Gateway
//!
//! The `oagw` gear fronts external HTTP services on behalf of platform
//! tenants. It is split into two cooperating planes:
//!
//! * **Control Plane** ([`domain::store`]) — tenant-scoped CRUD over
//!   upstreams, routes and plugins, plus alias derivation and hierarchical
//!   configuration resolution.
//! * **Data Plane** ([`infra::dp`]) — request execution: the plugin chain,
//!   rate limiting, header transformation, and the HTTP / SSE / WebSocket
//!   forwarders.
//!
//! Both planes are exposed over REST at `/oagw/v1/...`; see
//! [`docs/DESIGN.md`](../../../docs/DESIGN.md) §3.3 for the contract.
//!
//! # Storage
//!
//! The gear holds no database configuration: its `Cargo.toml` carries no
//! `toolkit-db` dependency and the gear registers the `rest` capability
//! only. State is therefore kept in an in-memory [`domain::store::Store`],
//! keyed by tenant. Restarting the process resets the roster, which is the
//! documented single-exec deployment mode.
pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;

pub use config::OagwConfig;

/// Crate name, used in logs and metrics.
pub const GEAR_NAME: &str = "oagw";
