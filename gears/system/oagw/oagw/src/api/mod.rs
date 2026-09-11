//! Transport layer of the OAGW gear: Axum routing, handlers and wire DTOs.
//!
//! The transport layer holds no business rule: it decodes the request, extracts
//! the tenant from the security context and delegates to the domain layer.

pub mod rest;
