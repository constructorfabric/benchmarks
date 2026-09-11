//! `oagw` — the outbound API gateway gear.
//!
//! The gear exposes two surfaces, both mounted at gear-relative paths:
//!
//! - a **management** (control-plane) API under `/oagw/v1/{upstreams,routes,plugins}`;
//! - a **proxy** (data-plane) API under `/oagw/v1/proxy/{alias}/{*path}`.
//!
//! Layering follows `docs/DESIGN.md` §3.2: the `domain` layer is free of
//! infrastructure types, `infra` implements the domain traits, and `api` maps
//! HTTP on to the domain.

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;

pub use gear::OagwGear;
