//! REST transport layer for the OAGW gear.
//!
//! Paths are gear-relative (`/oagw/v1/...`); the operator gateway in front of
//! the gear adds the `/api` prefix.

/// Wire DTOs.
pub mod dto;
/// Error mapping.
pub mod error;
/// Extractors.
pub mod extractors;
/// Handlers.
pub mod handlers;
/// Route registration.
pub mod routes;

pub use dto::{
    ListEnvelope, PluginCatalog, PluginRequestDto, PluginResponseDto, RouteRequestDto,
    RouteResponseDto, UpstreamRequestDto, UpstreamResponseDto,
};
pub use error::{ApiError, ERROR_SOURCE_HEADER, SOURCE_GATEWAY, SOURCE_UPSTREAM};
pub use extractors::{ApiState, ListParams, ResourceId, Tenant, client_ip};
pub use routes::{BASE, build_router, register_routes};
