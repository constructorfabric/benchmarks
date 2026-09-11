// Updated: 2026-09-01 by Constructor Tech
//! The Data Plane route: `{METHOD} /oagw/v1/proxy/{alias}[/{*path}]`.

use axum::Router;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationBuilder;

use crate::api::rest::handlers::proxy::{self, ProxyState};

const API_TAG: &str = "OAGW Proxy";

/// Register the proxy route for every method the design accepts.
pub fn register(router: Router, openapi: &dyn OpenApiRegistry, state: ProxyState) -> Router {
    // Any verb reaches the handler: the Data Plane classifies the request
    // itself, and a method the matched route rejects is its own 405.
    let router = router.route("/oagw/v1/proxy/{*path}", axum::routing::any(proxy::handle));

    // Documented, so the OpenAPI document describes what the gateway does.
    // Any method the caller sends is answered; a method the matched route
    // rejects becomes a 405 from the Data Plane.
    OperationBuilder::get("/oagw/v1/proxy/{*path}")
        .operation_id("oagw.proxy")
        .summary("Proxy a request")
        .description(
            "Forward a request to the upstream the alias names, applying the route's \
             plugin chain, header rules and rate limits. Streams server-sent events and \
             upgrades WebSockets.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param(
            "*path",
            "The alias, optionally followed by the path to forward",
        )
        .query_param("$filter", false, "unused")
        .handler(proxy::handle)
        .response(toolkit::api::operation_builder::ResponseSpec {
            status: http::StatusCode::OK.as_u16(),
            content_type: "*/*",
            description: "Whatever the upstream answered".to_owned(),
            schema: None,
        })
        .error_400(openapi)
        .error_500(openapi)
        .register(router, openapi)
        // The handler takes `ProxyState` from the extensions, and a layer only
        // covers the routes already added — so it goes on last, over both.
        .layer(axum::Extension(state))
}
