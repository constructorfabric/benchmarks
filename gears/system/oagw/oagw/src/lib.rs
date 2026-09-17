//! OAGW (Outbound API Gateway) gear.
//!
//! Centralized outbound proxy layer for external API calls. Provides:
//! - Control plane: CRUD management of `upstreams`, `routes`, and `plugins`
//! - Data plane: alias-based proxying at `/oagw/v1/proxy/{alias}[/{path}]` with
//!   credential injection (auth plugins), rate limiting, header
//!   transformation, CORS, and guard/transform plugin chains.
//!
//! The gear is registered in the process via [`gear::OagwGear`]. All HTTP
//! routes are under the `/oagw/v1` base (the api-gateway nests this router
//! under its own prefix).

pub mod config;
pub mod error;
pub mod gear;
pub mod gts_helpers;

mod api;
mod domain;
mod infra;

/// Re-exported domain service traits for integration tests and downstream
/// consumers that need to drive the gear without HTTP.
pub use domain::control_plane::ControlPlaneService;
pub use domain::data_plane::DataPlaneService;
pub use domain::model::{Protocol, Upstream};
pub use infra::storage::Services;
