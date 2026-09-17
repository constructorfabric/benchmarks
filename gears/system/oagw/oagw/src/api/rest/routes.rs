//! REST route registration for the OAGW gear.
//!
//! Paths are gear-relative (no `/api` prefix): the gear is mounted by the host
//! server and the wire paths are `/oagw/v1/…`.

use axum::Router;
use std::sync::Arc;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationBuilder;

use crate::api::rest::handlers;
use crate::domain::service::ControlPlaneService;
use crate::infra::state::GearState;
use crate::proxy::data_plane::DataPlane;
use crate::proxy::ratelimit::SharedRateLimiter;

const TAG: &str = "OAGW";

/// Registers every OAGW route on `router`.
///
/// The shared extensions are layered onto a dedicated sub-router so they stay
/// scoped to the OAGW surface: `Router::layer` only reaches routes registered
/// before it, and the host router already carries other gears' routes.
pub fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    state: GearState,
    service: ControlPlaneService,
    plane: Arc<DataPlane>,
    limiter: SharedRateLimiter,
) -> Router {
    let scoped = Router::new();
    let scoped = register_upstreams(scoped, openapi);
    let scoped = register_routes_crud(scoped, openapi);
    let scoped = register_plugins(scoped, openapi);
    let scoped = register_proxy(scoped, openapi);
    let scoped = scoped
        .layer(axum::Extension(plane))
        .layer(axum::Extension(state))
        .layer(axum::Extension(service))
        .layer(axum::Extension(limiter));
    router.merge(scoped)
}

fn register_upstreams(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    router = OperationBuilder::post("/oagw/v1/upstreams")
        .operation_id("oagw.create_upstream")
        .summary("Create an upstream")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<crate::api::rest::dto::UpsertUpstreamDto>(openapi, "Upstream definition")
        .handler(handlers::upstreams::create_upstream)
        .json_response_with_schema::<crate::api::rest::dto::UpstreamDto>(
            openapi,
            http::StatusCode::CREATED,
            "Created upstream",
        )
        .error_400(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams")
        .operation_id("oagw.list_upstreams")
        .summary("List upstreams")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .query_param_typed("top", false, "Maximum number of rows", "integer")
        .query_param_typed("skip", false, "Number of rows to skip", "integer")
        .query_param("alias", false, "Exact alias match")
        .handler(handlers::upstreams::list_upstreams)
        .json_array_response_with_schema::<crate::api::rest::dto::UpstreamDto>(
            openapi,
            http::StatusCode::OK,
            "Upstreams owned by the calling tenant",
        )
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.get_upstream")
        .summary("Get an upstream")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream identifier")
        .handler(handlers::upstreams::get_upstream)
        .json_response_with_schema::<crate::api::rest::dto::UpstreamDto>(
            openapi,
            http::StatusCode::OK,
            "Upstream found",
        )
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.replace_upstream")
        .summary("Replace an upstream")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream identifier")
        .json_request::<crate::api::rest::dto::UpsertUpstreamDto>(openapi, "Upstream definition")
        .handler(handlers::upstreams::replace_upstream)
        .json_response_with_schema::<crate::api::rest::dto::UpstreamDto>(
            openapi,
            http::StatusCode::OK,
            "Replaced upstream",
        )
        .error_400(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    OperationBuilder::delete("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.delete_upstream")
        .summary("Delete an upstream")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream identifier")
        .handler(handlers::upstreams::delete_upstream)
        .json_response(http::StatusCode::NO_CONTENT, "Upstream deleted")
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi)
}

fn register_routes_crud(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    router = OperationBuilder::post("/oagw/v1/routes")
        .operation_id("oagw.create_route")
        .summary("Create a route")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<crate::api::rest::dto::UpsertRouteDto>(openapi, "Route definition")
        .handler(handlers::routes::create_route)
        .json_response_with_schema::<crate::api::rest::dto::RouteDto>(
            openapi,
            http::StatusCode::CREATED,
            "Created route",
        )
        .error_400(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes")
        .operation_id("oagw.list_routes")
        .summary("List routes")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .query_param("upstream_id", false, "Restrict to an owning upstream")
        .query_param_typed("top", false, "Maximum number of rows", "integer")
        .query_param_typed("skip", false, "Number of rows to skip", "integer")
        .handler(handlers::routes::list_routes)
        .json_array_response_with_schema::<crate::api::rest::dto::RouteDto>(
            openapi,
            http::StatusCode::OK,
            "Routes owned by the calling tenant",
        )
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes/{id}")
        .operation_id("oagw.get_route")
        .summary("Get a route")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route identifier")
        .handler(handlers::routes::get_route)
        .json_response_with_schema::<crate::api::rest::dto::RouteDto>(
            openapi,
            http::StatusCode::OK,
            "Route found",
        )
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/routes/{id}")
        .operation_id("oagw.replace_route")
        .summary("Replace a route")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route identifier")
        .json_request::<crate::api::rest::dto::UpsertRouteDto>(openapi, "Route definition")
        .handler(handlers::routes::replace_route)
        .json_response_with_schema::<crate::api::rest::dto::RouteDto>(
            openapi,
            http::StatusCode::OK,
            "Replaced route",
        )
        .error_400(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    OperationBuilder::delete("/oagw/v1/routes/{id}")
        .operation_id("oagw.delete_route")
        .summary("Delete a route")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route identifier")
        .handler(handlers::routes::delete_route)
        .json_response(http::StatusCode::NO_CONTENT, "Route deleted")
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi)
}

fn register_plugins(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    router = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.create_plugin")
        .summary("Create a plugin")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<crate::api::rest::dto::CreatePluginDto>(openapi, "Plugin definition")
        .handler(handlers::plugins::create_plugin)
        .json_response_with_schema::<crate::api::rest::dto::PluginDto>(
            openapi,
            http::StatusCode::CREATED,
            "Created plugin",
        )
        .error_400(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins")
        .operation_id("oagw.list_plugins")
        .summary("List plugins")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .query_param("plugin_type", false, "Restrict to a plugin kind")
        .query_param_typed("top", false, "Maximum number of rows", "integer")
        .query_param_typed("skip", false, "Number of rows to skip", "integer")
        .handler(handlers::plugins::list_plugins)
        .json_array_response_with_schema::<crate::api::rest::dto::PluginDto>(
            openapi,
            http::StatusCode::OK,
            "Plugins owned by the calling tenant",
        )
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{id}")
        .operation_id("oagw.get_plugin")
        .summary("Get a plugin")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin identifier")
        .handler(handlers::plugins::get_plugin)
        .json_response_with_schema::<crate::api::rest::dto::PluginDto>(
            openapi,
            http::StatusCode::OK,
            "Plugin found",
        )
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{id}/source")
        .operation_id("oagw.get_plugin_source")
        .summary("Get plugin source")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin identifier")
        .handler(handlers::plugins::get_plugin_source)
        .json_response(http::StatusCode::OK, "Plugin source")
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    OperationBuilder::delete("/oagw/v1/plugins/{id}")
        .operation_id("oagw.delete_plugin")
        .summary("Delete a plugin")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin identifier")
        .handler(handlers::plugins::delete_plugin)
        .json_response(http::StatusCode::NO_CONTENT, "Plugin deleted")
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi)
}

fn register_proxy(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    // The proxy accepts every HTTP method; `Method::GET` is the documented
    // representative for the OpenAPI entry while the axum route is `any`.
    let router = OperationBuilder::new(http::Method::GET, "/oagw/v1/proxy/{alias}/{*path}")
        .operation_id("oagw.proxy")
        .summary("Proxy a request to an upstream")
        .description(
            "Forwards the request to the upstream resolved by alias. Accepts any HTTP method;              WebSocket upgrades and streamed bodies are passed through.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("alias", "Upstream alias")
        .path_param("path", "Path suffix appended to the matched route path")
        .method_router(axum::routing::any(handlers::proxy::proxy_with_suffix))
        .json_response(http::StatusCode::OK, "Proxied upstream response")
        .error_400(openapi)
        .error_404(openapi)
        .error_429(openapi)
        .error_500(openapi)
        .register(router, openapi);

    OperationBuilder::new(http::Method::GET, "/oagw/v1/proxy/{alias}")
        .operation_id("oagw.proxy_root")
        .summary("Proxy a request without a path suffix")
        .description(
            "Forwards the request to the upstream resolved by alias. Accepts any HTTP method.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("alias", "Upstream alias")
        .method_router(axum::routing::any(handlers::proxy::proxy_no_suffix))
        .json_response(http::StatusCode::OK, "Proxied upstream response")
        .error_400(openapi)
        .error_404(openapi)
        .error_429(openapi)
        .error_500(openapi)
        .register(router, openapi)
}
