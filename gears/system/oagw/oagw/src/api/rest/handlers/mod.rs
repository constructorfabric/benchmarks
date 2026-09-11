// Updated: 2026-09-01 by Constructor Tech
//! axum handlers.
//!
//! Each handler is a thin adapter: extract the caller's identity, call a
//! service, project the result onto the wire. Nothing here decides anything.

pub mod plugins;
pub mod proxy;
pub mod routes;
pub mod upstreams;
