//! oagw — `Oagw` gear: the Constructor Fabric outbound API gateway.
//!
//! The crate is split along DESIGN.md's DDD-Light layers:
//!
//! * [`domain`] — model, validation, alias derivation, route matching,
//!   configuration merging, rate limiting, header transformation, the plugin
//!   contracts and the Control Plane service. It depends on nothing from
//!   `infra` or `api`.
//! * [`infra`] — implementations of the domain ports: the in-memory
//!   repository, credential resolution, the outbound proxy leg and
//!   observability.
//! * [`api`] — the axum transport layer: DTOs, routes, handlers and the
//!   problem+json error mapping.
//!
//! Routes are registered gear-relative (`/oagw/v1/...`); the hosting
//! api-gateway adds its own prefix.
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod gts_helpers;
pub mod infra;

pub use gear::OagwGear;
