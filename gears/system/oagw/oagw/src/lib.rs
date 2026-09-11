//! OAGW — the outbound API gateway gear.
//!
//! The gear owns two planes:
//!
//! * a **control plane** (`/oagw/v1/upstreams`, `/oagw/v1/routes`, `/oagw/v1/plugins`) that stores
//!   upstream, route and plugin configuration per tenant, and
//! * a **data plane** (`/oagw/v1/proxy/{alias}/{*path}`) that resolves the caller's alias through
//!   the tenant hierarchy, matches a route, runs the plugin chain and forwards the request to the
//!   selected upstream endpoint.
//!
//! The behaviour contract lives in `gears/system/oagw/docs/` (PRD, DESIGN, ADR-0001…0009 and the
//! JSON Schemas). Storage is in-process behind the repository traits in [`domain::repo`]; the
//! graded configuration provisions no database for this gear.

#![allow(clippy::module_name_repetitions)]

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;

pub use gear::OagwGear;
