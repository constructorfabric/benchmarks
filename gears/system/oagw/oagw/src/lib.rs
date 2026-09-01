// Created: 2026-08-29 by Constructor Tech
//! OAGW — outbound API gateway gear.
//!
//! The gear exposes two planes over one crate (DESIGN `cpt-cf-oagw-component-model`):
//!
//! * **Control plane** — tenant-scoped CRUD over upstreams, routes and plugins
//!   (`/oagw/v1/upstreams`, `/oagw/v1/routes`, `/oagw/v1/plugins`).
//! * **Data plane** — request proxying (`/oagw/v1/proxy/{alias}/{path_suffix}`)
//!   for plain HTTP requests, server-sent-event streams and WebSocket upgrades.
//!
//! Layering follows DDD-Light: `domain` holds business logic without
//! infrastructure dependencies, `infra` holds implementations, `api/rest` holds
//! the axum transport layer, and `gear.rs` wires everything into the ToolKit
//! gear lifecycle.

// === MODULE DEFINITION ===
pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;

// === INTERNAL MODULES ===
#[doc(hidden)]
pub mod prelude;

pub use config::OagwConfig;
pub use gear::Oagw;
