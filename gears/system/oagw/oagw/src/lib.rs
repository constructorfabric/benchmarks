//! OAGW — Outbound API Gateway gear.
//!
//! Manages upstreams, routes, and plugins (control plane) and proxies
//! outbound traffic to configured upstreams (data plane).

/// REST layer (control-plane handlers; data-plane proxy handler lives in `infra::proxy`).
pub mod api;
/// Gear configuration.
pub mod config;
/// Domain model + control-plane service.
pub mod domain;
/// Gear declaration / registration with the ToolKit host.
pub mod gear;
/// Infrastructure: in-memory stores, proxy engine, plugin registry.
pub mod infra;

pub use crate::config::{OagwConfig, SsrfPolicy};
pub use crate::domain::service::ControlPlaneService;
pub use crate::gear::OagwGear;

/// `ToolKit` re-export used pervasively by OAGW code.
pub use toolkit;
