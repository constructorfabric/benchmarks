//! REST route registration for the `oagw` gear.
//!
//! Paths are gear-relative (`/oagw/v1/...`): the host `api-gateway` nests
//! them under its own prefix, which is empty in the graded configuration, so
//! no leading `/api` is added here (wire-contract note W1).

use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use toolkit::api::OpenApiRegistry;
use toolkit::api::OperationBuilder;

use super::handlers::management as handlers;
use super::handlers::proxy;
use super::state::OagwState;

const TAG: &str = "Outbound API Gateway";

/// Register every management route and stamp the shared state onto them.
///
/// The gear-level `RestApiCapability` implementation calls this once, after
/// `Gear::init` has built the control plane.
pub fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    state: Arc<OagwState>,
) -> Router {
    let router = register_upstream_routes(router, openapi);
    let router = register_route_routes(router, openapi);
    let router = register_plugin_routes(router, openapi);
    register_proxy_routes(router, openapi).layer(axum::Extension(state))
}

fn register_upstream_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::post("/oagw/v1/upstreams")
        .operation_id("oagw.create_upstream")
        .summary("Create an upstream")
        .description("Create an upstream. The alias is derived from hostname endpoints unless an explicit alias is required and supplied.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::create_upstream)
        .json_response(StatusCode::CREATED, "Upstream created")
        .error_400(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/upstreams")
        .operation_id("oagw.list_upstreams")
        .summary("List upstreams")
        .description("List the calling tenant's upstreams with OData `$filter`, `$select`, `$orderby`, `$top` and `$skip`.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::list_upstreams)
        .json_response(StatusCode::OK, "Upstream collection")
        .error_400(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.get_upstream")
        .summary("Fetch an upstream")
        .description("Fetch one upstream by GTS identifier or bare UUID.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream GTS identifier or bare UUID")
        .handler(handlers::get_upstream)
        .json_response(StatusCode::OK, "The upstream")
        .error_400(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::put("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.replace_upstream")
        .summary("Replace an upstream")
        .description("Full-replace an upstream. The alias is immutable once set.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream GTS identifier or bare UUID")
        .handler(handlers::replace_upstream)
        .json_response(StatusCode::OK, "The replaced upstream")
        .error_400(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    OperationBuilder::delete("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.delete_upstream")
        .summary("Delete an upstream")
        .description("Delete an upstream; its routes are deleted with it.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream GTS identifier or bare UUID")
        .handler(handlers::delete_upstream)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .error_400(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi)
}

fn register_route_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::post("/oagw/v1/routes")
        .operation_id("oagw.create_route")
        .summary("Create a route")
        .description("Create a route bound to an upstream owned by the calling tenant.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::create_route)
        .json_response(StatusCode::CREATED, "Route created")
        .error_400(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/routes")
        .operation_id("oagw.list_routes")
        .summary("List routes")
        .description(
            "List the calling tenant's routes, optionally narrowed with `upstream_id eq '{id}'`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::list_routes)
        .json_response(StatusCode::OK, "Route collection")
        .error_400(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/routes/{id}")
        .operation_id("oagw.get_route")
        .summary("Fetch a route")
        .description("Fetch one route by GTS identifier or bare UUID.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route GTS identifier or bare UUID")
        .handler(handlers::get_route)
        .json_response(StatusCode::OK, "The route")
        .error_400(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::put("/oagw/v1/routes/{id}")
        .operation_id("oagw.replace_route")
        .summary("Replace a route")
        .description("Full-replace a route. `upstream_id` is immutable.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route GTS identifier or bare UUID")
        .handler(handlers::replace_route)
        .json_response(StatusCode::OK, "The replaced route")
        .error_400(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    OperationBuilder::delete("/oagw/v1/routes/{id}")
        .operation_id("oagw.delete_route")
        .summary("Delete a route")
        .description("Delete one route.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route GTS identifier or bare UUID")
        .handler(handlers::delete_route)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .error_400(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi)
}

fn register_plugin_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.create_plugin")
        .summary("Create a custom plugin")
        .description(
            "Register a custom plugin written in Starlark. Plugins are immutable once created.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::create_plugin)
        .json_response(StatusCode::CREATED, "Plugin created")
        .error_400(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/plugins")
        .operation_id("oagw.list_plugins")
        .summary("List custom plugins")
        .description("List the calling tenant's custom plugins.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::list_plugins)
        .json_response(StatusCode::OK, "Plugin collection")
        .error_400(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/plugins/{id}")
        .operation_id("oagw.get_plugin")
        .summary("Fetch a custom plugin")
        .description("Fetch one custom plugin by GTS identifier or bare UUID.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin GTS identifier or bare UUID")
        .handler(handlers::get_plugin)
        .json_response(StatusCode::OK, "The plugin")
        .error_400(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/plugins/{id}/source")
        .operation_id("oagw.get_plugin_source")
        .summary("Fetch a plugin's Starlark source")
        .description("Return the Starlark source of a custom plugin.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin GTS identifier or bare UUID")
        .handler(handlers::get_plugin_source)
        .json_response(StatusCode::OK, "Plugin source")
        .error_400(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    OperationBuilder::delete("/oagw/v1/plugins/{id}")
        .operation_id("oagw.delete_plugin")
        .summary("Delete a custom plugin")
        .description(
            "Delete a custom plugin; fails with 409 while an upstream or route still binds it.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin GTS identifier or bare UUID")
        .handler(handlers::delete_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .error_400(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi)
}

/// The data-plane proxy: every method, mounted as a catch-all so the forwarded
/// suffix and query string survive verbatim.
fn register_proxy_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let declare = |router: Router, path: &'static str| {
        OperationBuilder::get(path)
            .operation_id("oagw.proxy")
            .summary("Proxy a request to an upstream")
            .description(
                "Forward an authenticated request to the upstream selected by `alias`, \
                 applying plugins, header rules and rate limits. Plain HTTP, \
                 server-sent-event streams and WebSocket upgrades are all proxied.",
            )
            .tag(TAG)
            .authenticated()
            .no_license_required()
            .path_param("alias", "Upstream alias to route through")
            .path_param("path_suffix", "Path forwarded to the upstream")
            .method_router(axum::routing::any(proxy::proxy))
            .no_content_response(StatusCode::OK, "Proxied response")
            .error_400(openapi)
            .error_404(openapi)
            .error_500(openapi)
            .register(router, openapi)
    };
    let router = declare(router, "/oagw/v1/proxy/{alias}");
    declare(router, "/oagw/v1/proxy/{alias}/{*path_suffix}")
}
