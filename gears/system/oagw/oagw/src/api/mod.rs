// Created: 2026-09-03 by Constructor Tech
//! REST surface of the OAGW gear, registered gear-relative at `/oagw/v1/...`.

pub mod dto;
pub mod handlers;
pub mod json;

use axum::routing::any;
use axum::Router;
use toolkit::api::{OpenApiRegistry, OperationBuilder};

use std::sync::Arc;

use crate::state::OagwState;

/// Registers every OAGW route on a fresh router.
///
/// Control-plane operations use the OpenAPI builder; the data plane is
/// registered as a catch-all `any` route because it accepts every method and
/// streams an opaque body. The shared state is layered last, so it reaches
/// every route added above (axum only layers existing routes).
///
/// # Errors
/// Propagates registry failures to the caller.
pub fn full_router(state: Arc<OagwState>, openapi: &dyn OpenApiRegistry) -> anyhow::Result<Router> {
    let router = Router::new();
    let router = upstream_routes(router, openapi);
    let router = route_routes(router, openapi);
    let router = plugin_routes(router, openapi);
    let router = data_plane_routes(router, openapi);
    Ok(router.layer(axum::Extension(state)))
}

fn upstream_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::post("/oagw/v1/upstreams")
        .operation_id("oagw.create_upstream")
        .summary("Create an upstream")
        .description(
            "Registers an outbound upstream. The alias is derived from the endpoint pool unless \
             it is explicitly provided.",
        )
        .tag("Outbound API Gateway")
        .json_request::<crate::model::UpstreamInput>(openapi, "The upstream definition")
        .authenticated()
        .no_license_required()
        .handler(handlers::create_upstream)
        .json_response_with_schema::<crate::model::Upstream>(
            openapi,
            http::StatusCode::CREATED,
            "The created upstream",
        )
        .error_400(openapi)
        .error_409(openapi)
        .register(router, openapi);
    let router = OperationBuilder::get("/oagw/v1/upstreams")
        .operation_id("oagw.list_upstreams")
        .summary("List upstreams")
        .description(
            "Returns the upstreams of the calling tenant, paged with OData query options.",
        )
        .tag("Outbound API Gateway")
        .query_param_typed("$top", false, "Maximum number of items", "integer")
        .query_param_typed("$skip", false, "Number of items to skip", "integer")
        .query_param("$filter", false, "OData filter expression")
        .query_param("$orderby", false, "Ordering expression")
        .query_param("$select", false, "Comma separated projection")
        .authenticated()
        .no_license_required()
        .handler(handlers::list_upstreams)
        .json_response_with_schema::<dto::ListEnvelope>(
            openapi,
            http::StatusCode::OK,
            "The matching upstreams",
        )
        .error_400(openapi)
        .register(router, openapi);
    let router = OperationBuilder::get("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.get_upstream")
        .summary("Fetch an upstream")
        .tag("Outbound API Gateway")
        .path_param("id", "GTS identifier of the upstream")
        .authenticated()
        .no_license_required()
        .handler(handlers::get_upstream)
        .json_response_with_schema::<crate::model::Upstream>(
            openapi,
            http::StatusCode::OK,
            "The upstream",
        )
        .error_404(openapi)
        .register(router, openapi);
    let router = OperationBuilder::put("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.replace_upstream")
        .summary("Replace an upstream")
        .tag("Outbound API Gateway")
        .path_param("id", "GTS identifier of the upstream")
        .json_request::<crate::model::UpstreamInput>(openapi, "The upstream definition")
        .authenticated()
        .no_license_required()
        .handler(handlers::update_upstream)
        .json_response_with_schema::<crate::model::Upstream>(
            openapi,
            http::StatusCode::OK,
            "The replaced upstream",
        )
        .error_400(openapi)
        .error_404(openapi)
        .register(router, openapi);
    OperationBuilder::delete("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.delete_upstream")
        .summary("Delete an upstream")
        .tag("Outbound API Gateway")
        .path_param("id", "GTS identifier of the upstream")
        .authenticated()
        .no_license_required()
        .handler(handlers::delete_upstream)
        .no_content_response(http::StatusCode::NO_CONTENT, "The upstream was deleted")
        .error_404(openapi)
        .register(router, openapi)
}

fn route_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::post("/oagw/v1/routes")
        .operation_id("oagw.create_route")
        .summary("Create a route")
        .description(
            "Attaches a matching rule to an upstream, making its alias reachable from the proxy \
             path.",
        )
        .tag("Outbound API Gateway")
        .json_request::<crate::model::RouteInput>(openapi, "The route definition")
        .authenticated()
        .no_license_required()
        .handler(handlers::create_route)
        .json_response_with_schema::<crate::model::Route>(
            openapi,
            http::StatusCode::CREATED,
            "The created route",
        )
        .error_400(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .register(router, openapi);
    let router = OperationBuilder::get("/oagw/v1/routes")
        .operation_id("oagw.list_routes")
        .summary("List routes")
        .tag("Outbound API Gateway")
        .query_param_typed("$top", false, "Maximum number of items", "integer")
        .query_param("$filter", false, "OData filter expression")
        .authenticated()
        .no_license_required()
        .handler(handlers::list_routes)
        .json_response_with_schema::<dto::ListEnvelope>(
            openapi,
            http::StatusCode::OK,
            "The matching routes",
        )
        .error_400(openapi)
        .register(router, openapi);
    let router = OperationBuilder::get("/oagw/v1/routes/{id}")
        .operation_id("oagw.get_route")
        .summary("Fetch a route")
        .tag("Outbound API Gateway")
        .path_param("id", "GTS identifier of the route")
        .authenticated()
        .no_license_required()
        .handler(handlers::get_route)
        .json_response_with_schema::<crate::model::Route>(
            openapi,
            http::StatusCode::OK,
            "The route",
        )
        .error_404(openapi)
        .register(router, openapi);
    let router = OperationBuilder::put("/oagw/v1/routes/{id}")
        .operation_id("oagw.replace_route")
        .summary("Replace a route")
        .tag("Outbound API Gateway")
        .path_param("id", "GTS identifier of the route")
        .json_request::<crate::model::RouteInput>(openapi, "The route definition")
        .authenticated()
        .no_license_required()
        .handler(handlers::update_route)
        .json_response_with_schema::<crate::model::Route>(
            openapi,
            http::StatusCode::OK,
            "The replaced route",
        )
        .error_400(openapi)
        .error_404(openapi)
        .register(router, openapi);
    OperationBuilder::delete("/oagw/v1/routes/{id}")
        .operation_id("oagw.delete_route")
        .summary("Delete a route")
        .tag("Outbound API Gateway")
        .path_param("id", "GTS identifier of the route")
        .authenticated()
        .no_license_required()
        .handler(handlers::delete_route)
        .no_content_response(http::StatusCode::NO_CONTENT, "The route was deleted")
        .error_404(openapi)
        .register(router, openapi)
}

fn plugin_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.create_plugin")
        .summary("Create a plugin definition")
        .description("Registers a declarative plugin that upstreams and routes can reference.")
        .tag("Outbound API Gateway")
        .json_request::<crate::model::PluginInput>(openapi, "The plugin definition")
        .authenticated()
        .no_license_required()
        .handler(handlers::create_plugin)
        .json_response_with_schema::<crate::model::PluginRecord>(
            openapi,
            http::StatusCode::CREATED,
            "The created plugin",
        )
        .error_400(openapi)
        .error_409(openapi)
        .register(router, openapi);
    let router = OperationBuilder::get("/oagw/v1/plugins")
        .operation_id("oagw.list_plugins")
        .summary("List plugins")
        .tag("Outbound API Gateway")
        .query_param_typed("$top", false, "Maximum number of items", "integer")
        .authenticated()
        .no_license_required()
        .handler(handlers::list_plugins)
        .json_response_with_schema::<dto::ListEnvelope>(
            openapi,
            http::StatusCode::OK,
            "The matching plugins",
        )
        .error_400(openapi)
        .register(router, openapi);
    let router = OperationBuilder::get("/oagw/v1/plugins/{id}")
        .operation_id("oagw.get_plugin")
        .summary("Fetch a plugin")
        .tag("Outbound API Gateway")
        .path_param("id", "GTS identifier of the plugin")
        .authenticated()
        .no_license_required()
        .handler(handlers::get_plugin)
        .json_response_with_schema::<crate::model::PluginRecord>(
            openapi,
            http::StatusCode::OK,
            "The plugin",
        )
        .error_404(openapi)
        .register(router, openapi);
    OperationBuilder::delete("/oagw/v1/plugins/{id}")
        .operation_id("oagw.delete_plugin")
        .summary("Delete a plugin")
        .description("Refuses with 409 while an upstream or route still references the plugin.")
        .tag("Outbound API Gateway")
        .path_param("id", "GTS identifier of the plugin")
        .authenticated()
        .no_license_required()
        .handler(handlers::delete_plugin)
        .no_content_response(http::StatusCode::NO_CONTENT, "The plugin was deleted")
        .error_404(openapi)
        .error_409(openapi)
        .register(router, openapi)
}

fn data_plane_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::new(http::Method::GET, "/oagw/v1/proxy/{alias}")
        .operation_id("oagw.proxy")
        .summary("Proxy an outbound request")
        .description(
            "Reverse-proxies the request to the endpoint pool of the alias. Every method is \
             accepted; streaming bodies and protocol upgrades pass through.",
        )
        .tag("Outbound API Gateway")
        .path_param("alias", "Routing alias of the upstream")
        .authenticated()
        .no_license_required()
        .method_router(any(handlers::proxy_alias))
        .problem_response(openapi, http::StatusCode::OK, "Proxied response")
        .error_400(openapi)
        .error_404(openapi)
        .error_429(openapi)
        .error_500(openapi)
        .register(router, openapi);
    OperationBuilder::new(http::Method::GET, "/oagw/v1/proxy/{alias}/{*path}")
        .operation_id("oagw.proxy_path")
        .summary("Proxy an outbound request with a path suffix")
        .tag("Outbound API Gateway")
        .path_param("alias", "Routing alias of the upstream")
        .path_param("path", "Path suffix forwarded to the upstream")
        .authenticated()
        .no_license_required()
        .method_router(any(handlers::proxy_suffix))
        .problem_response(openapi, http::StatusCode::OK, "Proxied response")
        .error_400(openapi)
        .error_404(openapi)
        .error_429(openapi)
        .error_500(openapi)
        .register(router, openapi)
}
