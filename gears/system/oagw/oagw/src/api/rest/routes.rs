//! `OperationBuilder` route registration.
//!
//! Paths are **gear-relative**: the api-gateway nests the composed router
//! under its own `prefix_path`, so registering `/api/...` here would double
//! the prefix. `DESIGN.md` tabulates the absolute paths behind an operator
//! gateway whose prefix is `/api`.

use std::sync::Arc;

use axum::Router;
use axum::routing::any;
use http::StatusCode;
use toolkit::api::{OpenApiRegistry, OperationBuilder};

use super::handlers::{self, OagwState};

const TAG: &str = "Outbound API Gateway";

const UPSTREAMS: &str = "/oagw/v1/upstreams";
const UPSTREAM: &str = "/oagw/v1/upstreams/{id}";
const ROUTES: &str = "/oagw/v1/routes";
const ROUTE: &str = "/oagw/v1/routes/{id}";
const PLUGINS: &str = "/oagw/v1/plugins";
const PLUGIN: &str = "/oagw/v1/plugins/{id}";
const PLUGIN_SOURCE: &str = "/oagw/v1/plugins/{id}/source";
const PROXY_ROOT: &str = "/oagw/v1/proxy/{alias}";
const PROXY_PATH: &str = "/oagw/v1/proxy/{alias}/{*path}";

/// Register the management and proxy surfaces.
#[allow(
    clippy::too_many_lines,
    reason = "one OperationBuilder chain per endpoint, in linear sequence"
)]
pub fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    state: Arc<OagwState>,
) -> Router {
    // -- upstreams ---------------------------------------------------------
    let router = OperationBuilder::post(UPSTREAMS)
        .operation_id("oagw.create_upstream")
        .summary("Create an upstream")
        .description(
            "Create an upstream for the calling tenant. The alias is auto-derived for \
             hostname-based endpoint pools and must be supplied explicitly for IP-based or \
             otherwise non-derivable pools.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::create_upstream)
        .json_response(StatusCode::CREATED, "Upstream created")
        .json_response(StatusCode::BAD_REQUEST, "Validation error")
        .json_response(StatusCode::CONFLICT, "Alias already exists for this tenant")
        .register(router, openapi);

    let router = OperationBuilder::get(UPSTREAMS)
        .operation_id("oagw.list_upstreams")
        .summary("List upstreams")
        .description("List the calling tenant's upstreams. Supports $filter/$select/$orderby/$top/$skip.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .query_param("$filter", false, "OData filter expression")
        .query_param("$select", false, "Comma-separated field list")
        .query_param("$orderby", false, "Sort order")
        .query_param_typed("$top", false, "Maximum results (default 50, max 100)", "integer")
        .query_param_typed("$skip", false, "Offset for pagination", "integer")
        .handler(handlers::list_upstreams)
        .json_response(StatusCode::OK, "Page of upstreams")
        .register(router, openapi);

    let router = OperationBuilder::get(UPSTREAM)
        .operation_id("oagw.get_upstream")
        .summary("Get an upstream")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream UUID or gts.cf.core.oagw.upstream.v1~{uuid}")
        .handler(handlers::get_upstream)
        .json_response(StatusCode::OK, "The upstream")
        .json_response(StatusCode::NOT_FOUND, "No such upstream for this tenant")
        .register(router, openapi);

    let router = OperationBuilder::put(UPSTREAM)
        .operation_id("oagw.replace_upstream")
        .summary("Replace an upstream")
        .description(
            "Full replacement: omitted optional members are cleared. The alias is immutable — \
             an endpoint change that would move it is rejected.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream UUID or gts.cf.core.oagw.upstream.v1~{uuid}")
        .handler(handlers::replace_upstream)
        .json_response(StatusCode::OK, "The replaced upstream")
        .json_response(StatusCode::BAD_REQUEST, "Validation error")
        .json_response(StatusCode::NOT_FOUND, "No such upstream for this tenant")
        .register(router, openapi);

    let router = OperationBuilder::delete(UPSTREAM)
        .operation_id("oagw.delete_upstream")
        .summary("Delete an upstream")
        .description("Deletes the upstream and cascades to its routes.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream UUID or gts.cf.core.oagw.upstream.v1~{uuid}")
        .handler(handlers::delete_upstream)
        .no_content_response(StatusCode::NO_CONTENT, "Upstream deleted")
        .json_response(StatusCode::NOT_FOUND, "No such upstream for this tenant")
        .register(router, openapi);

    // -- routes ------------------------------------------------------------
    let router = OperationBuilder::post(ROUTES)
        .operation_id("oagw.create_route")
        .summary("Create a route")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::create_route)
        .json_response(StatusCode::CREATED, "Route created")
        .json_response(StatusCode::BAD_REQUEST, "Validation error")
        .json_response(StatusCode::CONFLICT, "Duplicate match rule")
        .register(router, openapi);

    let router = OperationBuilder::get(ROUTES)
        .operation_id("oagw.list_routes")
        .summary("List routes")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .query_param("$filter", false, "OData filter expression")
        .query_param("$select", false, "Comma-separated field list")
        .query_param("$orderby", false, "Sort order")
        .query_param_typed("$top", false, "Maximum results (default 50, max 100)", "integer")
        .query_param_typed("$skip", false, "Offset for pagination", "integer")
        .handler(handlers::list_routes)
        .json_response(StatusCode::OK, "Page of routes")
        .register(router, openapi);

    let router = OperationBuilder::get(ROUTE)
        .operation_id("oagw.get_route")
        .summary("Get a route")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route UUID or gts.cf.core.oagw.route.v1~{uuid}")
        .handler(handlers::get_route)
        .json_response(StatusCode::OK, "The route")
        .json_response(StatusCode::NOT_FOUND, "No such route for this tenant")
        .register(router, openapi);

    let router = OperationBuilder::put(ROUTE)
        .operation_id("oagw.replace_route")
        .summary("Replace a route")
        .description("Full replacement. `upstream_id` is immutable.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route UUID or gts.cf.core.oagw.route.v1~{uuid}")
        .handler(handlers::replace_route)
        .json_response(StatusCode::OK, "The replaced route")
        .json_response(StatusCode::BAD_REQUEST, "Validation error")
        .json_response(StatusCode::NOT_FOUND, "No such route for this tenant")
        .register(router, openapi);

    let router = OperationBuilder::delete(ROUTE)
        .operation_id("oagw.delete_route")
        .summary("Delete a route")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route UUID or gts.cf.core.oagw.route.v1~{uuid}")
        .handler(handlers::delete_route)
        .no_content_response(StatusCode::NO_CONTENT, "Route deleted")
        .json_response(StatusCode::NOT_FOUND, "No such route for this tenant")
        .register(router, openapi);

    // -- plugins -----------------------------------------------------------
    let router = OperationBuilder::post(PLUGINS)
        .operation_id("oagw.create_plugin")
        .summary("Create a custom plugin")
        .description("Custom plugins are immutable after creation; there is no PUT.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::create_plugin)
        .json_response(StatusCode::CREATED, "Plugin created")
        .json_response(StatusCode::BAD_REQUEST, "Validation error")
        .json_response(StatusCode::CONFLICT, "Plugin name already used by this tenant")
        .register(router, openapi);

    let router = OperationBuilder::get(PLUGINS)
        .operation_id("oagw.list_plugins")
        .summary("List custom plugins")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .query_param("$filter", false, "OData filter expression")
        .query_param("$select", false, "Comma-separated field list")
        .query_param_typed("$top", false, "Maximum results (default 50, max 100)", "integer")
        .query_param_typed("$skip", false, "Offset for pagination", "integer")
        .handler(handlers::list_plugins)
        .json_response(StatusCode::OK, "Page of plugins")
        .register(router, openapi);

    let router = OperationBuilder::get(PLUGIN)
        .operation_id("oagw.get_plugin")
        .summary("Get a custom plugin")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "gts.cf.core.oagw.{type}_plugin.v1~{uuid} or a bare UUID")
        .handler(handlers::get_plugin)
        .json_response(StatusCode::OK, "The plugin")
        .json_response(StatusCode::NOT_FOUND, "No such plugin for this tenant")
        .register(router, openapi);

    let router = OperationBuilder::get(PLUGIN_SOURCE)
        .operation_id("oagw.get_plugin_source")
        .summary("Get a plugin's Starlark source")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "gts.cf.core.oagw.{type}_plugin.v1~{uuid} or a bare UUID")
        .handler(handlers::get_plugin_source)
        .text_response(StatusCode::OK, "Starlark source", "text/plain")
        .json_response(StatusCode::NOT_FOUND, "No such plugin for this tenant")
        .register(router, openapi);

    let router = OperationBuilder::delete(PLUGIN)
        .operation_id("oagw.delete_plugin")
        .summary("Delete a custom plugin")
        .description("Only unlinked plugins can be deleted; a referenced plugin returns 409.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "gts.cf.core.oagw.{type}_plugin.v1~{uuid} or a bare UUID")
        .handler(handlers::delete_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "Plugin deleted")
        .json_response(StatusCode::CONFLICT, "Plugin is still referenced")
        .json_response(StatusCode::NOT_FOUND, "No such plugin for this tenant")
        .register(router, openapi);

    // -- proxy -------------------------------------------------------------
    //
    // The proxy accepts every method, so the axum route is a single `any`
    // method-router; the OperationSpec below documents it (and puts it in the
    // gateway's authenticated-route matcher) once per path.
    let router = OperationBuilder::post(PROXY_ROOT)
        .operation_id("oagw.proxy_root")
        .summary("Proxy a request to an upstream")
        .description(
            "Forwards the request to the upstream resolved from {alias}, injecting credentials \
             and applying the configured plugin chain. Every method is accepted.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("alias", "Upstream alias")
        .method_router(any(handlers::proxy_root))
        .json_response(StatusCode::OK, "Upstream response (passthrough)")
        .json_response(StatusCode::NOT_FOUND, "No matching upstream or route")
        .register(router, openapi);

    let router = OperationBuilder::post(PROXY_PATH)
        .operation_id("oagw.proxy_path")
        .summary("Proxy a request to an upstream path")
        .description(
            "Forwards the request to the upstream resolved from {alias}, appending {path} to the \
             matched route's path when path_suffix_mode is `append`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("alias", "Upstream alias")
        .path_param("path", "Path suffix appended to the matched route path")
        .method_router(any(handlers::proxy_with_path))
        .json_response(StatusCode::OK, "Upstream response (passthrough)")
        .json_response(StatusCode::NOT_FOUND, "No matching upstream or route")
        .register(router, openapi);

    router.layer(axum::Extension(state))
}

#[cfg(test)]
#[path = "routes_tests.rs"]
mod tests;
