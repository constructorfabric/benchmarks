//! `oagw` — outbound API gateway gear.
//!
//! The gear lets tenant workloads call external services through a single
//! managed egress point: a management API for upstream/route/plugin CRUD and a
//! proxy data plane (`/oagw/v1/proxy/{alias}`) for the actual calls.
//!
//! The crate follows the DDD-Light layering used across gears
//! ([DESIGN.md](../../docs/DESIGN.md) `cpt-cf-oagw-design-layers`):
//!
//! - [`api`] — the REST adapters of the management API.
//! - [`config`] — deserialized `gears.oagw.config` block.
//! - [`domain`] — schema-mirroring model, domain errors and their canonical
//!   error / RFC 9457 `Problem` mapping. No framework types.
//! - [`infra`] — adapters, currently the in-memory per-tenant [`Store`].
//! - [`gear`] — the toolkit gear wiring config + store together.
//!

#![forbid(unsafe_code)]

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;

pub use config::{OagwConfig, SsrfPolicyConfig};
pub use domain::error::OagwError;
pub use gear::{OagwGear, OagwState};
pub use infra::store::Store;
