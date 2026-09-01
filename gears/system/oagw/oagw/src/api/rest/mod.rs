//! REST transport of the OAGW management surface (DESIGN §3.3 "Management
//! API").
//!
//! Handlers are plain axum handlers over [`crate::domain::services::management::ManagementService`]
//! and render their errors through [`crate::api::error::OagwError`], which carries
//! the OAGW error catalog GTS `type` identifiers.

pub mod body;
pub mod dto;
pub mod handlers;
pub mod odata;
pub mod proxy;
pub mod routes;

/// OpenAPI tag of the OAGW management surface.
pub const API_TAG: &str = "OAGW Upstreams";
