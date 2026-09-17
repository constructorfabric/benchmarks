//! OAGW — outbound API gateway gear (crate `cf-gears-oagw`, lib `oagw`).
//!
//! Gear foundation and package A scaffolding (feature
//! `cpt-cf-oagw-feature-gear-foundation`, DoD
//! `cpt-cf-oagw-dod-gear-foundation-skeleton`): the crate is laid out in
//! DDD-Light layers —
//!
//! - [`api`] — REST transport (`api/rest`): plane classification and the
//!   shared-state Extension; handlers arrive with the Control-Plane
//!   Management API (p3) and Data-Plane Proxy (p5) features;
//! - [`config`] — the `gears.oagw.config` module with documented defaults
//!   (DoD `cpt-cf-oagw-dod-gear-foundation-config`);
//! - [`domain`] — entities, repository contracts, merge semantics, error
//!   model, and the Control Plane / Data Plane service boundaries;
//! - [`infra`] — in-memory repositories, the RFC 9457 error envelope, and
//!   the canonical-error bridge;
//! - [`gear`] — the [`OagwGear`] ToolKit registration.
//!
//! Entry point: [`OagwGear`].

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;

pub use gear::OagwGear;
