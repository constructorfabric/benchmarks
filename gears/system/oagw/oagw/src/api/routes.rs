//! Route registration for the OAGW gear.

use std::sync::Arc;

use axum::Router;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationBuilder;

use crate::api::dto;
use crate::api::error::ApiContext;
use crate::api::handlers::{management, proxy};

/// Tag used by every management operation.
const API_TAG: &str = "OAGW";

/// Collection path of the upstream resource.
const UPSTREAMS: &str = "/oagw/v1/upstreams";
/// Item path of the upstream resource.
const UPSTREAM: &str = "/oagw/v1/upstreams/{id}";
/// Collection path of the route resource.
const ROUTES: &str = "/oagw/v1/routes";
/// Item path of the route resource.
const ROUTE: &str = "/oagw/v1/routes/{id}";
/// Collection path of the plugin resource.
const PLUGINS: &str = "/oagw/v1/plugins";
/// Item path of the plugin resource.
const PLUGIN: &str = "/oagw/v1/plugins/{id}";
/// Plugin source path.
const PLUGIN_SOURCE: &str = "/oagw/v1/plugins/{id}/source";
/// Data-plane configuration path.
const CONFIG: &str = "/oagw/v1/config";

/// Wire every OAGW REST route into the supplied router.
#[allow(
    clippy::too_many_lines,
    reason = "one OperationBuilder chain per endpoint, in linear sequence"
)]
pub fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    context: Arc<ApiContext>,
) -> Router {
    let router = management_routes(router, openapi);
    let router = config_routes(router, openapi);
    let router = proxy_routes(router, openapi);
    router.layer(axum::Extension(context))
}

/// Upstream, route and plugin CRUD.
fn management_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::post(UPSTREAMS)
        .operation_id("oagw.create_upstream")
        .summary("Create an upstream")
        .description(
            "Create an upstream for one tenant. The alias is derived from the \
             endpoint pool when it is omitted; an explicit alias must match the \
             derived value.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .handler(management::create_upstream)
        .json_response_with_schema::<dto::UpstreamDto>(
            openapi,
            http::StatusCode::CREATED,
            "The created upstream",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(UPSTREAMS)
        .operation_id("oagw.list_upstreams")
        .summary("List upstreams")
        .description("List the upstreams owned by the calling tenant.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .handler(management::list_upstreams)
        .json_array_response_with_schema::<dto::UpstreamDto>(
            openapi,
            http::StatusCode::OK,
            "The tenant's upstreams",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    let router = upstream_item_routes(router, openapi);
    let router = route_routes(router, openapi);
    plugin_routes(router, openapi)
}

/// Item routes of the upstream resource.
fn upstream_item_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::get(UPSTREAM)
        .operation_id("oagw.get_upstream")
        .summary("Get an upstream")
        .description("Read one upstream owned by the calling tenant.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream GTS identifier")
        .handler(management::get_upstream)
        .json_response_with_schema::<dto::UpstreamDto>(
            openapi,
            http::StatusCode::OK,
            "The requested upstream",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::put(UPSTREAM)
        .operation_id("oagw.replace_upstream")
        .summary("Replace an upstream")
        .description(
            "Replace an upstream. The alias is immutable: an endpoint change \
             that recomputes a different alias is rejected.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream GTS identifier")
        .json_request::<dto::UpstreamDto>(openapi, "The replacement upstream")
        .handler(management::replace_upstream)
        .json_response_with_schema::<dto::UpstreamDto>(
            openapi,
            http::StatusCode::OK,
            "The stored upstream",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    OperationBuilder::delete(UPSTREAM)
        .operation_id("oagw.delete_upstream")
        .summary("Delete an upstream")
        .description("Delete an upstream and the routes that reference it.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream GTS identifier")
        .handler(management::delete_upstream)
        .response(toolkit::api::ResponseSpec {
            status: http::StatusCode::NO_CONTENT.as_u16(),
            content_type: "application/json",
            description: "Deleted".to_owned(),
            schema: None,
        })
        .standard_errors(openapi)
        .register(router, openapi)
}

/// Route resource CRUD.
fn route_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::post(ROUTES)
        .operation_id("oagw.create_route")
        .summary("Create a route")
        .description("Create a route that matches proxied requests to an upstream.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<dto::RouteDto>(openapi, "The route to create")
        .handler(management::create_route)
        .json_response_with_schema::<dto::RouteDto>(
            openapi,
            http::StatusCode::CREATED,
            "The created route",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(ROUTES)
        .operation_id("oagw.list_routes")
        .summary("List routes")
        .description("List the routes owned by the calling tenant.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .handler(management::list_routes)
        .json_array_response_with_schema::<dto::RouteDto>(
            openapi,
            http::StatusCode::OK,
            "The tenant's routes",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(ROUTE)
        .operation_id("oagw.get_route")
        .summary("Get a route")
        .description("Read one route owned by the calling tenant.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route GTS identifier")
        .handler(management::get_route)
        .json_response_with_schema::<dto::RouteDto>(
            openapi,
            http::StatusCode::OK,
            "The requested route",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::put(ROUTE)
        .operation_id("oagw.replace_route")
        .summary("Replace a route")
        .description("Replace a route. The owning upstream is immutable.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route GTS identifier")
        .json_request::<dto::RouteDto>(openapi, "The replacement route")
        .handler(management::replace_route)
        .json_response_with_schema::<dto::RouteDto>(
            openapi,
            http::StatusCode::OK,
            "The stored route",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    OperationBuilder::delete(ROUTE)
        .operation_id("oagw.delete_route")
        .summary("Delete a route")
        .description("Delete a route owned by the calling tenant.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route GTS identifier")
        .handler(management::delete_route)
        .response(toolkit::api::ResponseSpec {
            status: http::StatusCode::NO_CONTENT.as_u16(),
            content_type: "application/json",
            description: "Deleted".to_owned(),
            schema: None,
        })
        .standard_errors(openapi)
        .register(router, openapi)
}

/// Plugin CRUD, which is create-only: plugins have no PUT.
fn plugin_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::post(PLUGINS)
        .operation_id("oagw.create_plugin")
        .summary("Create a plugin")
        .description("Register a tenant-defined plugin from its source text.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<dto::PluginDto>(openapi, "The plugin to register")
        .handler(management::create_plugin)
        .json_response_with_schema::<dto::PluginDto>(
            openapi,
            http::StatusCode::CREATED,
            "The registered plugin",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(PLUGINS)
        .operation_id("oagw.list_plugins")
        .summary("List plugins")
        .description("List the plugins owned by the calling tenant.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .handler(management::list_plugins)
        .json_array_response_with_schema::<dto::PluginDto>(
            openapi,
            http::StatusCode::OK,
            "The tenant's plugins",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(PLUGIN)
        .operation_id("oagw.get_plugin")
        .summary("Get a plugin")
        .description("Read one plugin owned by the calling tenant.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin GTS identifier")
        .handler(management::get_plugin)
        .json_response_with_schema::<dto::PluginDto>(
            openapi,
            http::StatusCode::OK,
            "The requested plugin",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(PLUGIN_SOURCE)
        .operation_id("oagw.get_plugin_source")
        .summary("Get the source of a plugin")
        .description("Read the source text stored for a plugin.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin GTS identifier")
        .handler(management::get_plugin_source)
        .json_response_with_schema::<dto::PluginSourceDto>(
            openapi,
            http::StatusCode::OK,
            "The stored source text",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    OperationBuilder::delete(PLUGIN)
        .operation_id("oagw.delete_plugin")
        .summary("Delete a plugin")
        .description("Delete an unreferenced plugin; referenced plugins conflict.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin GTS identifier")
        .handler(management::delete_plugin)
        .response(toolkit::api::ResponseSpec {
            status: http::StatusCode::NO_CONTENT.as_u16(),
            content_type: "application/json",
            description: "Deleted".to_owned(),
            schema: None,
        })
        .standard_errors(openapi)
        .register(router, openapi)
}

/// Effective data-plane configuration.
fn config_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    OperationBuilder::get(CONFIG)
        .operation_id("oagw.get_config")
        .summary("Get the effective data-plane configuration")
        .description("Read the limits the proxy enforces on proxied calls.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .handler(management::get_config)
        .json_response_with_schema::<management::ConfigDto>(
            openapi,
            http::StatusCode::OK,
            "The effective configuration",
        )
        .standard_errors(openapi)
        .register(router, openapi)
}

/// The proxy data plane, mounted for every method.
///
/// The operation is registered under `GET` so the gateway's auth policy
/// admits the path, while the supplied method router answers every method:
/// CORS preflights, regular calls and WebSocket upgrades all funnel into the
/// same handler.
fn proxy_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    OperationBuilder::new(http::Method::GET, proxy::PROXY_ROUTE)
        .operation_id("oagw.proxy")
        .summary("Proxy a request to an upstream")
        .description(
            "Proxy a request to the upstream selected by the alias. The remainder \
             of the path is handed to route matching; gateway errors are RFC 9457 \
             problem documents marked `X-OAGW-Error-Source: gateway`.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("rest", "Upstream alias followed by the path suffix")
        .response(toolkit::api::ResponseSpec {
            status: 200,
            content_type: "*/*",
            description: "Whatever the upstream returned".to_owned(),
            schema: None,
        })
        .method_router(axum::routing::any(proxy::proxy))
        .register(router, openapi)
}
