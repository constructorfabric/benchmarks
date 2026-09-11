//! The management surface: REST routes over the control plane.
//!
//! Handlers are thin: they parse the request, call the store, and map a domain error to
//! the canonical error catalog. The proxy endpoint is registered separately because it is
//! a pass-through relay, not a JSON operation.

pub mod dto;
pub mod handlers;
pub mod plugins;
pub mod proxy_endpoint;

use std::sync::Arc;

use axum::http::StatusCode;
use toolkit::api::{OpenApiRegistry, OperationBuilder};

use handlers::ConcreteService;

/// The `OpenAPI` tag the gear's operations carry.
pub const TAG: &str = "Outbound API Gateway";

/// Registers every management route.
pub fn register_routes(
    router: axum::Router,
    openapi: &dyn OpenApiRegistry,
    svc: Arc<ConcreteService>,
) -> axum::Router {
    let router = register_upstream_routes(router, openapi);
    let router = register_route_routes(router, openapi);
    let router = register_plugin_routes(router, openapi);
    let router = plugins::register(router, openapi);
    let router = proxy_endpoint::register(router, openapi);

    router.layer(axum::Extension(svc))
}

/// The `/oagw/v1/upstreams` collection and item operations.
fn register_upstream_routes(router: axum::Router, openapi: &dyn OpenApiRegistry) -> axum::Router {
    let router = OperationBuilder::get("/oagw/v1/upstreams")
        .operation_id("oagw.list_upstreams")
        .summary("List upstreams")
        .description("List the upstreams visible in the caller's tenant chain.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::list_upstreams)
        .json_response_with_schema::<dto::UpstreamList>(
            openapi,
            StatusCode::OK,
            "The visible upstreams",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::post("/oagw/v1/upstreams")
        .operation_id("oagw.create_upstream")
        .summary("Create an upstream")
        .description("Create an upstream in the caller's tenant, deriving its alias when absent.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<dto::UpstreamDto>(openapi, "The upstream definition")
        .handler(handlers::create_upstream)
        .no_content_response(StatusCode::CREATED, "Upstream created")
        .error_400(openapi)
        .error_401(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.get_upstream")
        .summary("Read an upstream")
        .description("Read one upstream by identifier.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::get_upstream)
        .json_response_with_schema::<dto::UpstreamDto>(
            openapi,
            StatusCode::OK,
            "The upstream",
        )
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::put("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.replace_upstream")
        .summary("Replace an upstream")
        .description(
            "Replace an upstream's configuration. The alias and owning tenant are immutable.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<dto::UpstreamDto>(openapi, "The replacement configuration")
        .handler(handlers::replace_upstream)
        .no_content_response(StatusCode::OK, "Upstream replaced")
        .error_400(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::delete("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.delete_upstream")
        .summary("Delete an upstream")
        .description("Delete an upstream and its routes.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::delete_upstream)
        .no_content_response(StatusCode::NO_CONTENT, "Upstream deleted")
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router
}

/// The `/oagw/v1/routes` collection, item and proxy operations.
fn register_route_routes(router: axum::Router, openapi: &dyn OpenApiRegistry) -> axum::Router {
    let router = OperationBuilder::get("/oagw/v1/routes")
        .operation_id("oagw.list_routes")
        .summary("List routes")
        .description("List the routes visible in the caller's tenant chain.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::list_routes)
        .json_response_with_schema::<dto::RouteList>(
            openapi,
            StatusCode::OK,
            "The visible routes",
        )
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::post("/oagw/v1/routes")
        .operation_id("oagw.create_route")
        .summary("Create a route")
        .description("Create a route on an upstream of the caller's tenant chain.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<dto::RouteDto>(openapi, "The route to create")
        .handler(handlers::create_route)
        .no_content_response(StatusCode::CREATED, "Route created")
        .error_400(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/routes/{id}")
        .operation_id("oagw.get_route")
        .summary("Read a route")
        .description("Read one route by identifier.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::get_route)
        .json_response_with_schema::<dto::RouteDto>(
            openapi,
            StatusCode::OK,
            "The route",
        )
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::put("/oagw/v1/routes/{id}")
        .operation_id("oagw.replace_route")
        .summary("Replace a route")
        .description("Replace a route's configuration; the upstream is immutable.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<dto::RouteDto>(openapi, "The replacement")
        .handler(handlers::replace_route)
        .no_content_response(StatusCode::OK, "Route replaced")
        .error_400(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::delete("/oagw/v1/routes/{id}")
        .operation_id("oagw.delete_route")
        .summary("Delete a route")
        .description("Delete a route.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::delete_route)
        .no_content_response(StatusCode::NO_CONTENT, "Route deleted")
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router
}

/// The `/oagw/v1/plugins` collection and item operations.
fn register_plugin_routes(router: axum::Router, openapi: &dyn OpenApiRegistry) -> axum::Router {
    let router = OperationBuilder::get("/oagw/v1/plugins")
        .operation_id("oagw.list_plugins")
        .summary("List plugins")
        .description("List the plugin definitions visible in the caller's tenant chain.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::list_plugins)
        .json_response_with_schema::<dto::PluginList>(
            openapi,
            StatusCode::OK,
            "The visible plugins",
        )
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.create_plugin")
        .summary("Create a plugin definition")
        .description("Create a named, immutable plugin definition.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<dto::PluginDto>(openapi, "The plugin definition")
        .handler(handlers::create_plugin)
        .no_content_response(StatusCode::CREATED, "Plugin created")
        .error_400(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/plugins/{id}")
        .operation_id("oagw.get_plugin")
        .summary("Read a plugin definition")
        .description("Read one plugin definition by identifier.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::get_plugin)
        .json_response_with_schema::<dto::PluginDto>(
            openapi,
            StatusCode::OK,
            "The plugin definition",
        )
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::delete("/oagw/v1/plugins/{id}")
        .operation_id("oagw.delete_plugin")
        .summary("Delete a plugin definition")
        .description("Delete a plugin definition that no upstream or route still binds.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::delete_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "Plugin deleted")
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router
}

#[cfg(test)]
#[path = "upstreams_tests.rs"]
mod upstreams_tests;

#[cfg(test)]
#[path = "routes_tests.rs"]
mod routes_tests;

#[cfg(test)]
#[path = "plugins_tests.rs"]
mod plugins_tests;

#[cfg(test)]
#[path = "proxy_endpoint_tests.rs"]
mod proxy_endpoint_tests;
