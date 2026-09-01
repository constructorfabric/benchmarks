//! Route registration for the OAGW management and proxy endpoints
//! (DESIGN §3.3).
//!
//! Wire paths use the platform-ingress prefix `/oagw/v1/...` (the earlier
//! `/api/...` prefix from DESIGN was dropped at platform level). All routes
//! require a bearer token (`.authenticated()`); the proxy route additionally
//! exposes an anonymous `OPTIONS` preflight (ADR-0004).

use std::sync::Arc;

use axum::Router;
use http::{Method, StatusCode};
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::{OperationBuilder, ResponseSpec};

use crate::api::rest::handlers;
use crate::domain::control::ControlPlaneService;
use crate::domain::model::{Plugin, Route, Upstream};
use crate::infra::data_plane::DataPlaneService;

/// `OpenAPI` tag for every OAGW operation.
const API_TAG: &str = "OAGW";

/// Proxy wildcard path shared by all verb registrations.
const PROXY_PATH: &str = "/oagw/v1/proxy/{alias}/{*path}";

/// Register every OAGW route on the router, attaching the shared service
/// extensions.
#[allow(clippy::needless_pass_by_value)]
pub fn register_routes(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
    control: Arc<ControlPlaneService>,
    data_plane: Arc<DataPlaneService>,
) -> Router {
    router = register_upstream_routes(router, openapi);
    router = register_route_routes(router, openapi);
    router = register_plugin_routes(router, openapi);
    router = register_proxy_routes(router, openapi);
    router = router
        .layer(axum::Extension(control))
        .layer(axum::Extension(data_plane));
    router
}

fn register_upstream_routes(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    router = OperationBuilder::get("/oagw/v1/upstreams")
        .operation_id("oagw.list_upstreams")
        .summary("List upstreams")
        .description("List the caller's upstreams with OData-style filtering, selection, ordering and offset pagination")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::list_upstreams)
        .json_array_response_with_schema::<Upstream>(
            openapi,
            StatusCode::OK,
            "List of upstreams",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/oagw/v1/upstreams")
        .operation_id("oagw.create_upstream")
        .summary("Create an upstream")
        .description("Create an upstream owned by the calling tenant; the alias is auto-derived from hostname endpoints")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::create_upstream)
        .json_request::<Upstream>(openapi, "Upstream to create")
        .json_response_with_schema::<Upstream>(
            openapi,
            StatusCode::CREATED,
            "Created upstream",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.get_upstream")
        .summary("Get an upstream")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "GTS instance id or bare UUID of the upstream")
        .handler(handlers::get_upstream)
        .json_response_with_schema::<Upstream>(openapi, StatusCode::OK, "Upstream")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.update_upstream")
        .summary("Replace an upstream")
        .description("Full replacement; the alias is immutable once set")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "GTS instance id or bare UUID of the upstream")
        .handler(handlers::update_upstream)
        .json_request::<Upstream>(openapi, "Replacement upstream")
        .json_response_with_schema::<Upstream>(openapi, StatusCode::OK, "Updated upstream")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.delete_upstream")
        .summary("Delete an upstream")
        .description("Delete an upstream and the routes that referenced it")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "GTS instance id or bare UUID of the upstream")
        .handler(handlers::delete_upstream)
        .no_content_response(StatusCode::NO_CONTENT, "Upstream deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router
}

fn register_route_routes(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    router = OperationBuilder::get("/oagw/v1/routes")
        .operation_id("oagw.list_routes")
        .summary("List routes")
        .description("List the caller's routes with OData-style filtering, selection, ordering and offset pagination")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::list_routes)
        .json_array_response_with_schema::<Route>(openapi, StatusCode::OK, "List of routes")
        .error_400(openapi)
        .error_401(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/oagw/v1/routes")
        .operation_id("oagw.create_route")
        .summary("Create a route")
        .description("Create a route on a caller-owned upstream")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::create_route)
        .json_request::<Route>(openapi, "Route to create")
        .json_response_with_schema::<Route>(openapi, StatusCode::CREATED, "Created route")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes/{id}")
        .operation_id("oagw.get_route")
        .summary("Get a route")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "GTS instance id or bare UUID of the route")
        .handler(handlers::get_route)
        .json_response_with_schema::<Route>(openapi, StatusCode::OK, "Route")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/routes/{id}")
        .operation_id("oagw.update_route")
        .summary("Replace a route")
        .description("Full replacement; the upstream binding is immutable")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "GTS instance id or bare UUID of the route")
        .handler(handlers::update_route)
        .json_request::<Route>(openapi, "Replacement route")
        .json_response_with_schema::<Route>(openapi, StatusCode::OK, "Updated route")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/routes/{id}")
        .operation_id("oagw.delete_route")
        .summary("Delete a route")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "GTS instance id or bare UUID of the route")
        .handler(handlers::delete_route)
        .no_content_response(StatusCode::NO_CONTENT, "Route deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router
}

fn register_plugin_routes(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    router = OperationBuilder::get("/oagw/v1/plugins")
        .operation_id("oagw.list_plugins")
        .summary("List plugins")
        .description("List the caller's custom plugins with OData-style filtering, selection, ordering and offset pagination")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::list_plugins)
        .json_array_response_with_schema::<Plugin>(openapi, StatusCode::OK, "List of plugins")
        .error_400(openapi)
        .error_401(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.create_plugin")
        .summary("Create a custom plugin")
        .description("Define a custom Starlark plugin; plugins are immutable after creation")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::create_plugin)
        .json_request::<Plugin>(openapi, "Plugin to create")
        .json_response_with_schema::<Plugin>(openapi, StatusCode::CREATED, "Created plugin")
        .error_400(openapi)
        .error_401(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{id}")
        .operation_id("oagw.get_plugin")
        .summary("Get a custom plugin")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "GTS instance id or bare UUID of the plugin")
        .handler(handlers::get_plugin)
        .json_response_with_schema::<Plugin>(openapi, StatusCode::OK, "Plugin")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{id}/source")
        .operation_id("oagw.get_plugin_source")
        .summary("Get the Starlark source of a custom plugin")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "GTS instance id or bare UUID of the plugin")
        .handler(handlers::get_plugin_source)
        .response(ResponseSpec {
            status: StatusCode::OK.as_u16(),
            content_type: "text/plain",
            description: "The Starlark source of the plugin".to_owned(),
            schema: None,
        })
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/plugins/{id}")
        .operation_id("oagw.delete_plugin")
        .summary("Delete a custom plugin")
        .description("Only unlinked plugins can be deleted; a referenced plugin returns 409")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "GTS instance id or bare UUID of the plugin")
        .handler(handlers::delete_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "Plugin deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router
}

fn register_proxy_routes(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    // Authenticated verb handlers (all share the same wildcard path; axum
    // merges them onto one route).
    for (method, name) in [
        (Method::GET, "proxy_get"),
        (Method::POST, "proxy_post"),
        (Method::PUT, "proxy_put"),
        (Method::DELETE, "proxy_delete"),
        (Method::PATCH, "proxy_patch"),
    ] {
        router = OperationBuilder::new(method.clone(), PROXY_PATH)
            .operation_id(format!("oagw.{name}"))
            .summary(format!("Proxy a {} request", method.as_str()))
            .description("Route the request to the upstream resolved for the alias")
            .tag(API_TAG)
            .authenticated()
            .no_license_required()
            .path_param("alias", "Upstream alias")
            .path_param("path", "Path suffix appended to the matched route path")
            .handler(handlers::proxy)
            .response(ResponseSpec {
                status: StatusCode::OK.as_u16(),
                content_type: "application/json",
                description: "Upstream response body (passthrough)".to_owned(),
                schema: None,
            })
            .error_400(openapi)
            .error_401(openapi)
            .error_403(openapi)
            .error_404(openapi)
            .error_429(openapi)
            .error_500(openapi)
            .error_502(openapi)
            .error_503(openapi)
            .error_504(openapi)
            .problem_response(
                openapi,
                StatusCode::PAYLOAD_TOO_LARGE,
                "Request payload too large",
            )
            .register(router, openapi);
    }

    // Anonymous CORS preflight (ADR-0004): permissive 204, no auth required.
    router = OperationBuilder::new(Method::OPTIONS, PROXY_PATH)
        .operation_id("oagw.proxy_preflight")
        .summary("CORS preflight for the proxy route")
        .tag(API_TAG)
        .anonymous()
        .path_param("alias", "Upstream alias")
        .path_param("path", "Path suffix appended to the matched route path")
        .method_router(axum::routing::options(handlers::proxy_preflight))
        .response(ResponseSpec {
            status: StatusCode::NO_CONTENT.as_u16(),
            content_type: "",
            description: "Permissive preflight response".to_owned(),
            schema: None,
        })
        .register(router, openapi);

    router
}
