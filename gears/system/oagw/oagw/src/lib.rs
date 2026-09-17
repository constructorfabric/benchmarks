//! OAGW — the outbound API gateway gear.
//!
//! A tenant-scoped reverse proxy in front of external services. The **control
//! plane** owns the configuration (upstreams, routes, plugins) and the **data
//! plane** executes proxied requests; both live in this crate and are reached
//! from REST under gear-relative `/oagw/v1/...` paths.
//!
//! * [`config`] — `gears.oagw.config`
//! * [`gear`] — the `#[toolkit::gear]` declaration and lifecycle
//! * [`domain`] — entities, errors, repository contracts, services, plugin traits
//! * [`infra`] — in-memory stores, the proxy engine and the built-in plugins
//! * [`api`] — REST transport layer

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;

pub use config::OagwConfig;
pub use gear::OagwGear;
