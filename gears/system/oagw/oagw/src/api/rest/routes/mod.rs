//! REST route registration for the `oagw` gear.

pub mod management;
pub mod proxy;

use std::sync::Arc;

use axum::Router;
use toolkit::api::OpenApiRegistry;

use crate::domain::services::OagwService;

/// Tag applied to every oagw operation in the OpenAPI document.
pub use management::API_TAG;

/// Registers every oagw REST route on the gear router.
///
/// # Errors
/// Returns an error when the data plane cannot be composed.
///
/// The management API and the data plane share the gear-relative `/oagw/v1`
/// prefix; the api-gateway nests the returned router under its own
/// `prefix_path`.
pub fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    service: Arc<OagwService>,
) -> anyhow::Result<Router> {
    let router = management::register_management_routes(router, openapi, service.clone());
    proxy::register_proxy_routes(router, openapi, &service)
}
