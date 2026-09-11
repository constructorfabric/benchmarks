//! REST route registration of the proxy data plane (FEATURE entry 2.4).
//!
//! The two proxy paths are registered as a catch-all for **every** HTTP
//! method and are deliberately **not** documented in the OpenAPI document:
//! the proxy surface relays whatever the caller addresses at an alias, so its
//! requests and responses are the upstream's, not the gear's
//! (`inst-rp-dispatch-1` .. `-4`). The handler parses the request target
//! itself, because a path suffix may itself contain any segment shape.
//!
//! The router carries the [`crate::infra::proxy::DataPlaneServiceImpl`] handle
//! as extension state, layered on this registration only: a layer applied to
//! the router as handed over would not cover the paths this entry adds.

use std::sync::Arc;

use axum::Router;
use toolkit::api::OpenApiRegistry;

use super::proxy_handlers;
use crate::infra::proxy::DataPlaneServiceImpl;

/// Register the proxy catch-all of `/oagw/v1/proxy/{alias}[/{*path_suffix}]`.
///
/// Both paths accept every HTTP method and reach the same handler, which
/// detects a CORS preflight, resolves the caller, gates the permission and
/// dispatches. No OpenAPI operation is emitted.
pub fn register_proxy_routes(
    router: Router,
    _openapi: &dyn OpenApiRegistry,
    service: Arc<DataPlaneServiceImpl>,
) -> Router {
    // @cpt-begin:cpt-cf-oagw-flow-request-proxy-dispatch:p1:inst-rp-dispatch-2
    // `inst-rp-dispatch-1` .. `-4`: the catch-all is gear-relative
    // (`/oagw/v1/proxy/...`, no `/api` segment), registered for every HTTP
    // method, and not an OpenAPI-documented operation. The handler parses the
    // alias and the path suffix out of the request target itself, so the
    // suffix may carry any segment shape the caller sends.
    router
        .route(
            super::PROXY_PATH,
            axum::routing::any(proxy_handlers::proxy),
        )
        .route(
            super::PROXY_SUFFIX_PATH,
            axum::routing::any(proxy_handlers::proxy),
        )
        .layer(axum::Extension(service))
    // @cpt-end:cpt-cf-oagw-flow-request-proxy-dispatch:p1:inst-rp-dispatch-2
}
