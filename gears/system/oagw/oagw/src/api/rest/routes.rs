//! Route registration.
//!
//! Paths are registered **gear-relative** (`/oagw/v1/...`): the API gateway
//! nests a gear's router under its own `prefix_path`, so a gear that spelled
//! the prefix itself would answer on `/{prefix}/{prefix}/…` and 404 on every
//! documented route.
//!
//! The proxy endpoints are mounted with `any(...)` because the proxy is
//! method-transparent: whatever verb the caller used is the verb the upstream
//! sees. `OPTIONS` must reach the handler too — that is where the CORS
//! preflight is answered (`cpt-cf-oagw-adr-cors`).

use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use axum::routing::any;
use toolkit::api::{OpenApiRegistry, OperationBuilder, ensure_schema};

use crate::api::rest::handlers::{self, AppState};
use crate::domain::dto::{PluginSpec, RouteSpec, UpstreamSpec};

const TAG: &str = "Outbound API Gateway";

/// Management API base path (gear-relative).
pub const UPSTREAMS_PATH: &str = "/oagw/v1/upstreams";
/// Management API base path for routes.
pub const ROUTES_PATH: &str = "/oagw/v1/routes";
/// Management API base path for plugins.
pub const PLUGINS_PATH: &str = "/oagw/v1/plugins";
/// Proxy path for a bare alias.
pub const PROXY_ROOT_PATH: &str = "/oagw/v1/proxy/{alias}";
/// Proxy path with a path suffix.
pub const PROXY_PATH: &str = "/oagw/v1/proxy/{alias}/{*path}";

/// Register every OAGW route on `router`.
#[allow(clippy::too_many_lines, reason = "one builder chain per endpoint")]
pub fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    state: Arc<AppState>,
) -> Router {
    let upstream_schema = ensure_schema::<UpstreamSpec>(openapi);
    let route_schema = ensure_schema::<RouteSpec>(openapi);
    let plugin_schema = ensure_schema::<PluginSpec>(openapi);

    // ---- upstreams -------------------------------------------------------
    let router = OperationBuilder::post(UPSTREAMS_PATH)
        .operation_id("oagw.create_upstream")
        .summary("Create an upstream")
        .description(
            "Register an external service. The alias is auto-derived for hostname-based \
             endpoint pools and must be supplied explicitly for IP-based or otherwise \
             non-derivable pools.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request_schema(upstream_schema.clone(), "Upstream configuration")
        .handler(handlers::create_upstream)
        .json_response(StatusCode::CREATED, "Upstream created")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(UPSTREAMS_PATH)
        .operation_id("oagw.list_upstreams")
        .summary("List upstreams")
        .description("List the calling tenant's upstreams. Supports $filter, $select, $orderby, $top and $skip.")
        .tag(TAG)
        .query_param("$filter", false, "OData filter expression")
        .query_param("$select", false, "Comma-separated fields to return")
        .query_param("$orderby", false, "Sort order, e.g. `alias desc`")
        .query_param_typed("$top", false, "Maximum results (default 50, max 100)", "integer")
        .query_param_typed("$skip", false, "Offset for pagination", "integer")
        .authenticated()
        .no_license_required()
        .handler(handlers::list_upstreams)
        .json_response(StatusCode::OK, "A page of upstreams")
        .error_400(openapi)
        .error_401(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.get_upstream")
        .summary("Get an upstream")
        .tag(TAG)
        .path_param(
            "id",
            "Upstream UUID or `gts.cf.core.oagw.upstream.v1~{uuid}`",
        )
        .authenticated()
        .no_license_required()
        .handler(handlers::get_upstream)
        .json_response(StatusCode::OK, "The upstream")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::put("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.replace_upstream")
        .summary("Replace an upstream")
        .description(
            "Full replacement: omitted optional blocks are cleared. The alias is immutable — \
             an endpoint change that would derive a different alias is rejected.",
        )
        .tag(TAG)
        .path_param(
            "id",
            "Upstream UUID or `gts.cf.core.oagw.upstream.v1~{uuid}`",
        )
        .authenticated()
        .no_license_required()
        .json_request_schema(upstream_schema, "Upstream configuration")
        .handler(handlers::replace_upstream)
        .json_response(StatusCode::OK, "The replaced upstream")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::delete("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.delete_upstream")
        .summary("Delete an upstream")
        .description("Deletes the upstream and cascades to its routes.")
        .tag(TAG)
        .path_param(
            "id",
            "Upstream UUID or `gts.cf.core.oagw.upstream.v1~{uuid}`",
        )
        .authenticated()
        .no_license_required()
        .handler(handlers::delete_upstream)
        .no_content_response(StatusCode::NO_CONTENT, "Upstream deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // ---- routes ----------------------------------------------------------
    let router = OperationBuilder::post(ROUTES_PATH)
        .operation_id("oagw.create_route")
        .summary("Create a route")
        .description("Register an API path on an upstream the calling tenant owns.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request_schema(route_schema.clone(), "Route configuration")
        .handler(handlers::create_route)
        .json_response(StatusCode::CREATED, "Route created")
        .error_400(openapi)
        .error_401(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(ROUTES_PATH)
        .operation_id("oagw.list_routes")
        .summary("List routes")
        .description("List the calling tenant's routes. Supports $filter, $select, $orderby, $top and $skip.")
        .tag(TAG)
        .query_param("$filter", false, "OData filter expression")
        .query_param("$select", false, "Comma-separated fields to return")
        .query_param("$orderby", false, "Sort order")
        .query_param_typed("$top", false, "Maximum results (default 50, max 100)", "integer")
        .query_param_typed("$skip", false, "Offset for pagination", "integer")
        .authenticated()
        .no_license_required()
        .handler(handlers::list_routes)
        .json_response(StatusCode::OK, "A page of routes")
        .error_400(openapi)
        .error_401(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/routes/{id}")
        .operation_id("oagw.get_route")
        .summary("Get a route")
        .tag(TAG)
        .path_param("id", "Route UUID or `gts.cf.core.oagw.route.v1~{uuid}`")
        .authenticated()
        .no_license_required()
        .handler(handlers::get_route)
        .json_response(StatusCode::OK, "The route")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::put("/oagw/v1/routes/{id}")
        .operation_id("oagw.replace_route")
        .summary("Replace a route")
        .description("Full replacement. `upstream_id` is immutable.")
        .tag(TAG)
        .path_param("id", "Route UUID or `gts.cf.core.oagw.route.v1~{uuid}`")
        .authenticated()
        .no_license_required()
        .json_request_schema(route_schema, "Route configuration")
        .handler(handlers::replace_route)
        .json_response(StatusCode::OK, "The replaced route")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::delete("/oagw/v1/routes/{id}")
        .operation_id("oagw.delete_route")
        .summary("Delete a route")
        .tag(TAG)
        .path_param("id", "Route UUID or `gts.cf.core.oagw.route.v1~{uuid}`")
        .authenticated()
        .no_license_required()
        .handler(handlers::delete_route)
        .no_content_response(StatusCode::NO_CONTENT, "Route deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // ---- plugins ---------------------------------------------------------
    let router = OperationBuilder::post(PLUGINS_PATH)
        .operation_id("oagw.create_plugin")
        .summary("Create a custom plugin")
        .description("Plugins are immutable after creation; publish a new one to change behaviour.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request_schema(plugin_schema, "Plugin definition")
        .handler(handlers::create_plugin)
        .json_response(StatusCode::CREATED, "Plugin created")
        .error_400(openapi)
        .error_401(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(PLUGINS_PATH)
        .operation_id("oagw.list_plugins")
        .summary("List custom plugins")
        .tag(TAG)
        .query_param("$filter", false, "OData filter expression")
        .query_param("$select", false, "Comma-separated fields to return")
        .query_param_typed(
            "$top",
            false,
            "Maximum results (default 50, max 100)",
            "integer",
        )
        .query_param_typed("$skip", false, "Offset for pagination", "integer")
        .authenticated()
        .no_license_required()
        .handler(handlers::list_plugins)
        .json_response(StatusCode::OK, "A page of plugins")
        .error_400(openapi)
        .error_401(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/plugins/{id}")
        .operation_id("oagw.get_plugin")
        .summary("Get a custom plugin")
        .tag(TAG)
        .path_param(
            "id",
            "Plugin UUID or `gts.cf.core.oagw.{type}_plugin.v1~{uuid}`",
        )
        .authenticated()
        .no_license_required()
        .handler(handlers::get_plugin)
        .json_response(StatusCode::OK, "The plugin")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/plugins/{id}/source")
        .operation_id("oagw.get_plugin_source")
        .summary("Get a plugin's source")
        .tag(TAG)
        .path_param(
            "id",
            "Plugin UUID or `gts.cf.core.oagw.{type}_plugin.v1~{uuid}`",
        )
        .authenticated()
        .no_license_required()
        .handler(handlers::get_plugin_source)
        .text_response(StatusCode::OK, "The plugin source", "text/plain")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::delete("/oagw/v1/plugins/{id}")
        .operation_id("oagw.delete_plugin")
        .summary("Delete a custom plugin")
        .description("Only unlinked plugins can be deleted; a referenced plugin yields 409.")
        .tag(TAG)
        .path_param(
            "id",
            "Plugin UUID or `gts.cf.core.oagw.{type}_plugin.v1~{uuid}`",
        )
        .authenticated()
        .no_license_required()
        .handler(handlers::delete_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "Plugin deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // ---- proxy -----------------------------------------------------------
    let proxy_description = "Forward a request to the aliased upstream. Every method is \
                             accepted and passed through, including SSE responses and \
                             WebSocket upgrades. `OPTIONS` with `Origin` and \
                             `Access-Control-Request-Method` is answered locally as a CORS \
                             preflight.";

    let router = OperationBuilder::post(PROXY_ROOT_PATH)
        .operation_id("oagw.proxy_root")
        .summary("Proxy a request to an upstream")
        .description(proxy_description)
        .tag(TAG)
        .path_param("alias", "Upstream alias (case-insensitive)")
        .authenticated()
        .no_license_required()
        .method_router(any(handlers::proxy_root))
        .json_response(StatusCode::OK, "The upstream response, passed through")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_429(openapi)
        .error_500(openapi)
        .error_502(openapi)
        .error_503(openapi)
        .error_504(openapi)
        .register(router, openapi);

    let router = OperationBuilder::post(PROXY_PATH)
        .operation_id("oagw.proxy")
        .summary("Proxy a request to an upstream path")
        .description(proxy_description)
        .tag(TAG)
        .path_param("alias", "Upstream alias (case-insensitive)")
        .path_param("path", "Path suffix forwarded to the upstream")
        .authenticated()
        .no_license_required()
        .method_router(any(handlers::proxy_path))
        .json_response(StatusCode::OK, "The upstream response, passed through")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_429(openapi)
        .error_500(openapi)
        .error_502(openapi)
        .error_503(openapi)
        .error_504(openapi)
        .register(router, openapi);

    router.layer(axum::Extension(state))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_gear_relative() {
        for path in [
            UPSTREAMS_PATH,
            ROUTES_PATH,
            PLUGINS_PATH,
            PROXY_ROOT_PATH,
            PROXY_PATH,
        ] {
            assert!(
                path.starts_with("/oagw/v1/"),
                "{path} must be gear-relative: the gateway adds its own prefix"
            );
            assert!(
                !path.starts_with("/api/"),
                "{path} must not repeat the gateway prefix"
            );
        }
    }

    #[test]
    fn proxy_paths_use_axum_wildcard_syntax() {
        assert_eq!(PROXY_ROOT_PATH, "/oagw/v1/proxy/{alias}");
        assert_eq!(PROXY_PATH, "/oagw/v1/proxy/{alias}/{*path}");
    }
}
