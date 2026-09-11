//! OAGW — Outbound API Gateway gear.
//!
//! This crate establishes OAGW as a loadable gear: it registers with the
//! host runtime (`cpt-cf-oagw-dod-gear-registration`), resolves its
//! configuration once at startup (`cpt-cf-oagw-dod-config-resolution`),
//! mounts its router gear-relative at `/oagw/v1/...`
//! (`cpt-cf-oagw-dod-router-mount`), initializes an in-process
//! control-plane store (`cpt-cf-oagw-dod-control-plane-state`), and defines
//! the shared RFC 9457 error contract every later feature builds on
//! (`cpt-cf-oagw-dod-problem-details-shape`,
//! `cpt-cf-oagw-dod-error-status-mapping`,
//! `cpt-cf-oagw-dod-error-source-header`).
//!
//! This feature registers no concrete management or proxy resource
//! endpoint; later features
//! (`cpt-cf-oagw-feature-upstream-management`,
//! `cpt-cf-oagw-feature-route-management`,
//! `cpt-cf-oagw-feature-plugin-management`,
//! `cpt-cf-oagw-feature-alias-resolution`,
//! `cpt-cf-oagw-feature-http-proxy`,
//! `cpt-cf-oagw-feature-streaming-proxy`,
//! `cpt-cf-oagw-feature-plugin-runtime`) extend
//! [`api::rest::routes::register_routes`] with their own endpoints.

#![forbid(unsafe_code)]

pub mod api;
pub mod config;
pub mod domain;
pub mod error;
pub mod gear;
pub mod state;

pub use domain::{Plugin, Route, Upstream};
pub use error::{OagwError, OagwErrorKind};
pub use gear::OagwGear;
pub use state::ControlPlaneState;
