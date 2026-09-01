// Created: 2026-08-29 by Constructor Tech
//! REST route registration.
//!
//! The management API is documented once through `OperationBuilder` under the
//! gear-relative `/oagw/v1/...` prefix, and the same handlers are re-registered
//! as undocumented axum routes under `/api/oagw/v1/...` because the DESIGN doc
//! spells the paths with the `/api` prefix. Both prefixes must work.

use std::sync::Arc;

use axum::Router;
use toolkit::api::{OpenApiRegistry, OperationBuilder};

use super::handlers::{self, Services};
use crate::api::rest::dto::{
    CreatePluginBody, CreateRouteBody, CreateUpstreamBody, PluginDto, PluginList, ReplaceRouteBody,
    ReplaceUpstreamBody, RouteDto, RouteList, UpstreamDto, UpstreamList,
};

const TAG: &str = "Outbound API Gateway";

/// Gear-relative prefix of every documented OAGW operation.
const BASE: &str = "/oagw/v1";

/// DESIGN-doc prefix, served by the same handlers.
const API_BASE: &str = "/api/oagw/v1";

/// Register every REST route for the OAGW gear.
pub fn register(router: Router, openapi: &dyn OpenApiRegistry, services: Arc<Services>) -> Router {
    router
        .merge(
            management_routes(Router::new(), openapi).layer(axum::Extension(Arc::clone(&services))),
        )
        .merge(undocumented_management().layer(axum::Extension(Arc::clone(&services))))
        .merge(proxy_routes().layer(axum::Extension(services)))
}

fn upstream_id_path(suffix: &str) -> String {
    format!("{BASE}/upstreams/{{id}}{suffix}")
}

fn api_upstream_id_path(suffix: &str) -> String {
    format!("{API_BASE}/upstreams/{{id}}{suffix}")
}

fn management_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::post(format!("{BASE}/upstreams"))
        .operation_id("oagw.create_upstream")
        .summary("Create an upstream")
        .description(
            "Register an outbound service. The alias is derived from the endpoints; \
             endpoints that are not derivable require an explicit `alias`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<CreateUpstreamBody>(openapi, "Upstream to create")
        .handler(handlers::create_upstream)
        .json_response_with_schema::<UpstreamDto>(
            openapi,
            axum::http::StatusCode::CREATED,
            "Created upstream",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(format!("{BASE}/upstreams"))
        .operation_id("oagw.list_upstreams")
        .summary("List upstreams")
        .description(
            "List upstreams of the calling tenant with OData `$top` / `$skip` / \
             `$filter` / `$select` / `$orderby`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::list_upstreams)
        .json_response_with_schema::<UpstreamList>(openapi, axum::http::StatusCode::OK, "Page")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(upstream_id_path(""))
        .operation_id("oagw.get_upstream")
        .summary("Get an upstream")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream identifier")
        .handler(handlers::get_upstream)
        .json_response_with_schema::<UpstreamDto>(openapi, axum::http::StatusCode::OK, "Upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::put(upstream_id_path(""))
        .operation_id("oagw.replace_upstream")
        .summary("Replace an upstream")
        .description("Full replace. `alias` is immutable; a change is rejected with 400.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream identifier")
        .json_request::<ReplaceUpstreamBody>(openapi, "Replacement upstream")
        .handler(handlers::replace_upstream)
        .json_response_with_schema::<UpstreamDto>(
            openapi,
            axum::http::StatusCode::OK,
            "Replaced upstream",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::delete(upstream_id_path(""))
        .operation_id("oagw.delete_upstream")
        .summary("Delete an upstream")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream identifier")
        .handler(handlers::delete_upstream)
        .no_content_response(axum::http::StatusCode::NO_CONTENT, "Deleted")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = enable_disable(router, openapi);
    let router = route_operations(router, openapi);
    plugin_operations(router, openapi)
}

fn enable_disable(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::post(upstream_id_path("/enable"))
        .operation_id("oagw.enable_upstream")
        .summary("Enable an upstream")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream identifier")
        .handler(handlers::enable_upstream)
        .json_response_with_schema::<UpstreamDto>(
            openapi,
            axum::http::StatusCode::OK,
            "Upstream enabled",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    OperationBuilder::post(upstream_id_path("/disable"))
        .operation_id("oagw.disable_upstream")
        .summary("Disable an upstream")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream identifier")
        .handler(handlers::disable_upstream)
        .json_response_with_schema::<UpstreamDto>(
            openapi,
            axum::http::StatusCode::OK,
            "Upstream disabled",
        )
        .standard_errors(openapi)
        .register(router, openapi)
}

fn route_operations(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::post(format!("{BASE}/routes"))
        .operation_id("oagw.create_route")
        .summary("Create a route")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<CreateRouteBody>(openapi, "Route to create")
        .handler(handlers::create_route)
        .json_response_with_schema::<RouteDto>(
            openapi,
            axum::http::StatusCode::CREATED,
            "Created route",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(format!("{BASE}/routes"))
        .operation_id("oagw.list_routes")
        .summary("List routes")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::list_routes)
        .json_response_with_schema::<RouteList>(openapi, axum::http::StatusCode::OK, "Page")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(format!("{BASE}/routes/{{id}}"))
        .operation_id("oagw.get_route")
        .summary("Get a route")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route identifier")
        .handler(handlers::get_route)
        .json_response_with_schema::<RouteDto>(openapi, axum::http::StatusCode::OK, "Route")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::put(format!("{BASE}/routes/{{id}}"))
        .operation_id("oagw.replace_route")
        .summary("Replace a route")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route identifier")
        .json_request::<ReplaceRouteBody>(openapi, "Replacement route")
        .handler(handlers::replace_route)
        .json_response_with_schema::<RouteDto>(
            openapi,
            axum::http::StatusCode::OK,
            "Replaced route",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    OperationBuilder::delete(format!("{BASE}/routes/{{id}}"))
        .operation_id("oagw.delete_route")
        .summary("Delete a route")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route identifier")
        .handler(handlers::delete_route)
        .no_content_response(axum::http::StatusCode::NO_CONTENT, "Deleted")
        .standard_errors(openapi)
        .register(router, openapi)
}

fn plugin_operations(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::post(format!("{BASE}/plugins"))
        .operation_id("oagw.create_plugin")
        .summary("Register a custom plugin")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<CreatePluginBody>(openapi, "Plugin to register")
        .handler(handlers::create_plugin)
        .json_response_with_schema::<PluginDto>(
            openapi,
            axum::http::StatusCode::CREATED,
            "Registered plugin",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(format!("{BASE}/plugins"))
        .operation_id("oagw.list_plugins")
        .summary("List plugins")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::list_plugins)
        .json_response_with_schema::<PluginList>(openapi, axum::http::StatusCode::OK, "Page")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(format!("{BASE}/plugins/{{id}}"))
        .operation_id("oagw.get_plugin")
        .summary("Get a plugin")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin identifier")
        .handler(handlers::get_plugin)
        .json_response_with_schema::<PluginDto>(openapi, axum::http::StatusCode::OK, "Plugin")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::delete(format!("{BASE}/plugins/{{id}}"))
        .operation_id("oagw.delete_plugin")
        .summary("Delete a plugin")
        .description(
            "Fails with 409 and a `referenced_by` body when an upstream or route still \
             references the plugin.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin identifier")
        .handler(handlers::delete_plugin)
        .no_content_response(axum::http::StatusCode::NO_CONTENT, "Deleted")
        .standard_errors(openapi)
        .register(router, openapi);

    OperationBuilder::get(format!("{BASE}/plugins/{{id}}/source"))
        .operation_id("oagw.get_plugin_source")
        .summary("Get plugin source")
        .description("Returns the Starlark source of a custom plugin as `text/plain`.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin identifier")
        .handler(handlers::get_plugin_source)
        .no_content_response(axum::http::StatusCode::OK, "Starlark source")
        .standard_errors(openapi)
        .register(router, openapi)
}

/// The same management handlers under the DESIGN-doc prefix.
fn undocumented_management() -> Router {
    use axum::routing::{get, post};
    Router::new()
        .route(
            &format!("{API_BASE}/upstreams"),
            post(handlers::create_upstream).get(handlers::list_upstreams),
        )
        .route(
            &api_upstream_id_path(""),
            get(handlers::get_upstream)
                .put(handlers::replace_upstream)
                .delete(handlers::delete_upstream),
        )
        .route(
            &api_upstream_id_path("/enable"),
            post(handlers::enable_upstream),
        )
        .route(
            &api_upstream_id_path("/disable"),
            post(handlers::disable_upstream),
        )
        .route(
            &format!("{API_BASE}/routes"),
            post(handlers::create_route).get(handlers::list_routes),
        )
        .route(
            &format!("{API_BASE}/routes/{{id}}"),
            get(handlers::get_route)
                .put(handlers::replace_route)
                .delete(handlers::delete_route),
        )
        .route(
            &format!("{API_BASE}/plugins"),
            post(handlers::create_plugin).get(handlers::list_plugins),
        )
        .route(
            &format!("{API_BASE}/plugins/{{id}}"),
            get(handlers::get_plugin).delete(handlers::delete_plugin),
        )
        .route(
            &format!("{API_BASE}/plugins/{{id}}/source"),
            get(handlers::get_plugin_source),
        )
}

/// The data-plane proxy endpoints.
///
/// `/proxy/{alias}` and `/proxy/{alias}/{*path_suffix}` are `any` routes so
/// every method and an SSE / WebSocket upgrade reach the same handler.
fn proxy_routes() -> Router {
    const SUFFIX_PATH: &str = "{alias}/{*path_suffix}";
    Router::new()
        .route(
            &format!("{BASE}/proxy/{SUFFIX_PATH}"),
            axum::routing::any(handlers::proxy::proxy_with_suffix),
        )
        .route(
            &format!("{BASE}/proxy/{{alias}}"),
            axum::routing::any(handlers::proxy::proxy),
        )
        .route(
            &format!("{API_BASE}/proxy/{SUFFIX_PATH}"),
            axum::routing::any(handlers::proxy::proxy_with_suffix),
        )
        .route(
            &format!("{API_BASE}/proxy/{{alias}}"),
            axum::routing::any(handlers::proxy::proxy),
        )
}
