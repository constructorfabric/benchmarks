//! Handlers for the 15 management routes (DESIGN §3.3 "Management API").
//!
//! Every handler resolves the calling tenant from the
//! `toolkit_security::context::SecurityContext` extension, delegates to
//! [`crate::domain::service::ControlPlaneService`] and converts the result to
//! a wire response. Errors propagate as [`crate::domain::error::DomainError`],
//! which renders as `application/problem+json` (see `super::error`).

pub use crate::api::rest::common;
pub mod plugins;
pub mod proxy;
pub mod routes;
pub mod upstreams;
