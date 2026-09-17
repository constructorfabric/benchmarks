//! `oagw` — outbound API gateway gear.
//!
//! The gear exposes a tenant-scoped management API for upstreams, routes and
//! plugins and a data plane that proxies plain HTTP, server-sent events and
//! WebSocket upgrades to configured upstream services.
//!
//! ```ignore
//! let gear = OagwGear::default();
//! gear.init(&ctx).await?;
//! // routes are mounted by the host through `RestApiCapability::register_rest`
//! ```

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod ids;
pub mod infra;

pub use config::OagwConfig;
pub use gear::OagwGear;
