//! OAGW REST API: DTOs, route registration and handlers.

pub mod dto;
pub mod handlers;
pub mod routes;

/// Tags used in the `OpenAPI` document.
pub(crate) const TAG_UPSTREAM: &str = "OAGW Upstreams";
pub(crate) const TAG_ROUTE: &str = "OAGW Routes";
pub(crate) const TAG_PLUGIN: &str = "OAGW Plugins";
pub(crate) const TAG_PROXY: &str = "OAGW Data Plane";
