//! Outbound API gateway gear.
//!
//! `oagw` proxies every outbound call the platform makes to an external
//! service. It owns a control plane, which manages upstream, route and plugin
//! configuration through a management API, and a data plane, which resolves
//! that configuration and forwards the request.
//!
//! Routes are registered gear-relative under `/oagw/v1`. The api-gateway gear
//! nests its own single global prefix over the whole assembled router, so this
//! gear must not repeat that prefix itself.

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;

pub use config::OagwConfig;
pub use gear::Oagw;
