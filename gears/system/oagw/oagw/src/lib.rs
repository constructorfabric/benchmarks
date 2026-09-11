//! OAGW — Outbound API Gateway gear.
//!
//! The gear exposes two surfaces over gear-relative `/oagw/v1` paths:
//!
//! * a **Control Plane** (`/oagw/v1/upstreams`, `/oagw/v1/routes`,
//!   `/oagw/v1/plugins`) where operators configure upstream endpoint pools,
//!   routes and plugin bindings; and
//! * a **Data Plane** (`/oagw/v1/proxy/{alias}/...`) which resolves the alias,
//!   matches a route, runs the plugin chain, injects credentials resolved from
//!   the credential store by reference, and forwards the request to the
//!   upstream — streaming plain HTTP, server-sent events and WebSocket
//!   upgrades alike.
//!
//! Layering follows DDD-Light: [`api`] is the transport layer,
//! [`domain`] holds the model, services and repository traits with no
//! infrastructure dependency, and [`infra`] implements the domain traits.
//!
//! Route registration is gear-relative: the gear never repeats the host-level
//! path prefix, which the api-gateway adds when it nests the shared router
//! under its own `prefix_path`.
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![allow(clippy::module_name_repetitions)]

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod gts_helpers;
pub mod infra;
