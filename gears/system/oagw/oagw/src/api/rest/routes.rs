//! REST route registration for the oagw gear.
//!
//! Every path here is gear-relative and complete: the gear's routes live under
//! `/oagw/v1/...`, with no leading `/api`. Management operations are
//! authenticated; the proxy is authenticated too, because its alias resolution
//! walks the caller's tenant chain and is therefore meaningless without an
//! identity (a preflight is still answered anonymously by the host's auth
//! layer, which is what ADR-0004 asks for).

use std::sync::Arc;

use axum::{Router, routing::any};
use toolkit::api::OpenApiRegistry;
use toolkit::api::canonical_prelude::StatusCode;
use toolkit::api::operation_builder::OperationBuilder;

use super::handlers::{plugins, proxy, routes_api, upstreams};
use crate::domain::services::ControlPlaneService;

const API_TAG: &str = "Outbound API Gateway";
const PROXY_TAG: &str = "Outbound API Gateway proxy";

/// Registers every REST route the gear serves.
#[allow(clippy::needless_pass_by_value)]
pub fn register_routes(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
    service: Arc<ControlPlaneService>,
    proxy: Arc<proxy::ProxyState>,
) -> Router {
    router = register_upstreams(router, openapi);
    router = register_routes_api(router, openapi);
    router = register_plugins(router, openapi);
    register_proxy(router, openapi, service, proxy)
}

fn register_upstreams(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    router = OperationBuilder::post("/oagw/v1/upstreams")
        .operation_id("oagw.create_upstream")
        .summary("Create an upstream")
        .description(
            "Registers an upstream. The alias is derived from the server host when it is omitted.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request_schema("Upstream", "The upstream to register")
        .handler(upstreams::create)
        .json_response(StatusCode::CREATED, "The upstream was created")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams")
        .operation_id("oagw.list_upstreams")
        .summary("List upstreams")
        .description("Lists the tenant's upstreams, ordered by alias, as one page.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .query_param("offset", false, "Offset the page starts at")
        .query_param("limit", false, "Maximum number of items in the page")
        .query_param(
            "$count",
            false,
            "Ask for the total number of matching items",
        )
        .handler(upstreams::list)
        .json_response(StatusCode::OK, "A page of upstreams")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams/{upstream_id}")
        .operation_id("oagw.get_upstream")
        .summary("Read an upstream")
        .description("Returns a single upstream by identifier.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("upstream_id", "Identifier of the upstream")
        .handler(upstreams::get)
        .json_response(StatusCode::OK, "The upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/upstreams/{upstream_id}")
        .operation_id("oagw.replace_upstream")
        .summary("Replace an upstream")
        .description("Replaces an upstream in full; the alias may not be changed.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("upstream_id", "Identifier of the upstream")
        .json_request_schema("Upstream", "The replacement upstream")
        .handler(upstreams::replace)
        .json_response(StatusCode::OK, "The replaced upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/upstreams/{upstream_id}")
        .operation_id("oagw.delete_upstream")
        .summary("Delete an upstream")
        .description("Deletes an upstream together with its routes.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("upstream_id", "Identifier of the upstream")
        .handler(upstreams::delete)
        .no_content_response(StatusCode::NO_CONTENT, "The upstream is gone")
        .standard_errors(openapi)
        .register(router, openapi);

    router
}

fn register_routes_api(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    router = OperationBuilder::post("/oagw/v1/routes")
        .operation_id("oagw.create_route_standalone")
        .summary("Create a route")
        .description("Registers a match rule; the body names the owning upstream.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request_schema("Route", "The route to register")
        .handler(routes_api::create_standalone)
        .json_response(StatusCode::CREATED, "The route was created")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/oagw/v1/upstreams/{upstream_id}/routes")
        .operation_id("oagw.create_route")
        .summary("Create a route under an upstream")
        .description("Registers a match rule under the upstream named in the path.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("upstream_id", "Identifier of the owning upstream")
        .json_request_schema("Route", "The route to register")
        .handler(routes_api::create)
        .json_response(StatusCode::CREATED, "The route was created")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes")
        .operation_id("oagw.list_routes")
        .summary("List routes")
        .description("Lists the tenant's routes, highest priority first, as one page.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .query_param("offset", false, "Offset the page starts at")
        .query_param("limit", false, "Maximum number of items in the page")
        .query_param(
            "$count",
            false,
            "Ask for the total number of matching items",
        )
        .handler(routes_api::list)
        .json_response(StatusCode::OK, "A page of routes")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes/{route_id}")
        .operation_id("oagw.get_route")
        .summary("Read a route")
        .description("Returns a single route by identifier.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("route_id", "Identifier of the route")
        .handler(routes_api::get)
        .json_response(StatusCode::OK, "The route")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/routes/{route_id}")
        .operation_id("oagw.replace_route")
        .summary("Replace a route")
        .description("Replaces a route in full; its upstream may not be changed.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("route_id", "Identifier of the route")
        .json_request_schema("Route", "The replacement route")
        .handler(routes_api::replace)
        .json_response(StatusCode::OK, "The replaced route")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/routes/{route_id}")
        .operation_id("oagw.delete_route")
        .summary("Delete a route")
        .description("Deletes a single route.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("route_id", "Identifier of the route")
        .handler(routes_api::delete)
        .no_content_response(StatusCode::NO_CONTENT, "The route is gone")
        .standard_errors(openapi)
        .register(router, openapi);

    router
}

fn register_plugins(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    router = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.create_plugin")
        .summary("Register a plugin")
        .description(
            "Registers a custom plugin. Its source is stored and returned by the source endpoint.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request_schema("Plugin", "The plugin to register")
        .handler(plugins::create)
        .json_response(StatusCode::CREATED, "The plugin was registered")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins")
        .operation_id("oagw.list_plugins")
        .summary("List plugins")
        .description("Returns the plugin catalogue: the built-ins plus the tenant's plugins.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .handler(plugins::list)
        .json_response(StatusCode::OK, "The plugin catalogue")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{plugin_id}")
        .operation_id("oagw.get_plugin")
        .summary("Read a plugin")
        .description("Returns a single plugin by identifier, built-ins included.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("plugin_id", "Identifier of the plugin")
        .handler(plugins::get)
        .json_response(StatusCode::OK, "The plugin")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{plugin_id}/source")
        .operation_id("oagw.get_plugin_source")
        .summary("Read a plugin's source")
        .description("Returns the stored source text of a custom plugin.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("plugin_id", "Identifier of the plugin")
        .handler(plugins::source)
        .json_response(StatusCode::OK, "The plugin source")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/plugins/{plugin_id}")
        .operation_id("oagw.delete_plugin")
        .summary("Delete a plugin")
        .description("Deletes a custom plugin.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("plugin_id", "Identifier of the plugin")
        .handler(plugins::delete)
        .no_content_response(StatusCode::NO_CONTENT, "The plugin is gone")
        .standard_errors(openapi)
        .register(router, openapi);

    router
}

fn register_proxy(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    service: Arc<ControlPlaneService>,
    proxy_state: Arc<proxy::ProxyState>,
) -> Router {
    // The proxy accepts every verb, so the method router is composed by hand
    // and handed to the builder, which only documents it.
    let any_path = any(proxy::proxy);
    let any_prefixed = any(proxy::proxy);

    let router = OperationBuilder::new(axum::http::Method::GET, "/oagw/v1/proxy/{alias}")
        .operation_id("oagw.proxy")
        .summary("Proxy a request")
        .description(
            "Proxies a request to the upstream the alias names. Streams the response body, \
             including `text/event-stream` and WebSocket upgrades.",
        )
        .tag(PROXY_TAG)
        .authenticated()
        .no_license_required()
        .path_param("alias", "Alias of the upstream to call")
        .json_response(StatusCode::OK, "Whatever the upstream answered")
        .standard_errors(openapi)
        .method_router(any_path)
        .register(router, openapi);

    let router = OperationBuilder::new(axum::http::Method::GET, "/oagw/v1/proxy/{alias}/{*path}")
        .operation_id("oagw.proxy_path")
        .summary("Proxy a request with a path")
        .description(
            "Proxies a request with a path suffix to the upstream the alias names. Streams the \
             response body, including `text/event-stream` and WebSocket upgrades.",
        )
        .tag(PROXY_TAG)
        .authenticated()
        .no_license_required()
        .path_param("alias", "Alias of the upstream to call")
        .path_param("path", "Path to forward to the upstream")
        .json_response(StatusCode::OK, "Whatever the upstream answered")
        .standard_errors(openapi)
        .method_router(any_prefixed)
        .register(router, openapi);

    router
        .layer(axum::Extension(proxy_state))
        .layer(axum::Extension(service))
}
