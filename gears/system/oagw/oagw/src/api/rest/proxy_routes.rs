//! REST registration for the proxy path (DESIGN §3.5 "Proxy API").
//!
//! The proxy accepts **any** method: the methods a route allows are a
//! configuration decision taken by the route matcher, not by the transport, so
//! the transport must forward every method it receives (including `PATCH`,
//! `OPTIONS` and future RFC methods) and let the domain reject it with 404.

use axum::Router;
use http::Method;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationBuilder;

use super::proxy_handlers::{PROXY_PREFIX, proxy_alias, proxy_alias_with_path};
use crate::domain::services::data_plane::DataPlaneService;

/// Tag used for the proxy operations in the OpenAPI document.
const API_TAG: &str = "Outbound API Gateway";

/// Registers the two proxy paths: the alias alone and the alias with a
/// sub-path.
pub fn register_proxy_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    service: std::sync::Arc<DataPlaneService>,
) -> Router {
    // ANY /oagw/v1/proxy/{alias}
    let alias_path = format!("{PROXY_PREFIX}/{{alias}}");
    let alias_sub_path = format!("{PROXY_PREFIX}/{{alias}}/{{*path}}");

    let router = OperationBuilder::new(Method::GET, alias_path)
        .operation_id("oagw.proxy_alias")
        .summary("Proxy a request by alias")
        .description(
            "Forward a request to the upstream resolved from the alias. The request is matched \
             against the routes of the upstream; any HTTP method is accepted and validated by \
             the matched route.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("alias", "Upstream alias, e.g. api.openai.com")
        .method_router(axum::routing::any(proxy_alias))
        .problem_response(
            openapi,
            http::StatusCode::NOT_FOUND,
            "No enabled upstream or route matches",
        )
        .register(router, openapi);

    // ANY /oagw/v1/proxy/{alias}/{*path}
    let router = OperationBuilder::new(Method::GET, alias_sub_path)
        .operation_id("oagw.proxy_alias_path")
        .summary("Proxy a request by alias and path")
        .description(
            "Forward a request to the upstream resolved from the alias. `path` is the part of \
             the request path after the alias and is matched against the routes of the \
             upstream; any HTTP method is accepted and validated by the matched route.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("alias", "Upstream alias, e.g. api.openai.com")
        .path_param("path", "Path forwarded to the upstream, e.g. v1/chat")
        .method_router(axum::routing::any(proxy_alias_with_path))
        .problem_response(
            openapi,
            http::StatusCode::NOT_FOUND,
            "No enabled upstream or route matches",
        )
        .register(router, openapi);

    // The proxy service extension is attached as an explicit final step over the
    // whole router, for the same reason as in `routes::register_routes`:
    // `Router::layer` only wraps the routes registered so far.
    router.layer(axum::Extension(service))
}
