//! OAGW — Outbound API Gateway.
//!
//! A framework gear that manages outbound upstream access for tenants:
//! control-plane CRUD over upstreams/routes/plugins, a rate-limited,
//! CORS-aware, plugin-chained data-plane proxy, and GTS type provisioning.
//!
//! Module layout follows DESIGN §3.2:
//! - [`config`] — gear configuration (`gears.oagw.config`).
//! - [`domain`] — domain model, GTS ids, error contract, services.
//! - [`infra`] — storage, plugin builtins/registries, data plane, GTS
//!   type provisioning.
//! - [`api`] — REST surface (`/oagw/v1/...`).
//! - [`gear`] — gear wiring (toolkit gear macro).
//!
//! The gear is rest-only (no lifecycle) because the frozen dependency set
//! precludes `tokio-util`; see `gear.rs` and the completion report.

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod gts;
pub mod infra;

pub use gear::OagwGear;
