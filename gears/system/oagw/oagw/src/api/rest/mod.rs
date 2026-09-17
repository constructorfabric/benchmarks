//! REST API layer for the OAGW gear.
//!
//! * [`dto`] — wire shapes for the management API.
//! * [`extractors`] — path/query/tenant extractors.
//! * [`error`] — `DomainError` → RFC 9457 problem document mapping.
//! * [`handlers`] — the handlers themselves.
//! * [`routes_reg`] — the `OperationBuilder` registration chain.

use std::sync::Arc;

use axum::Router;
use toolkit::api::OpenApiRegistry;

use crate::domain::services::ManagementService;
use crate::infra::proxy::pipeline::DataPlaneService;

pub mod dto;
pub mod error;
pub mod extractors;
pub mod handlers;
pub mod routes_reg;

/// Registers the OAGW management API and the proxy data plane on `router`.
///
/// The [`ManagementService`] and the [`DataPlaneService`] are installed as
/// `axum::Extension`s, so every handler can extract them (and the handlers are
/// the only places that do).
pub fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    service: Arc<ManagementService>,
    data_plane: Arc<DataPlaneService>,
) -> Router {
    routes_reg::register_routes(router, openapi, service, data_plane)
}
