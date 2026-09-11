//! Route registration.
//!
//! Paths are **gear-relative**: api-gateway nests this router under its own
//! `prefix_path`, so registering `/api/oagw/v1/...` here would double the
//! prefix and 404 every documented route. `docs/PRD.md` tabulates the
//! absolute paths an operator gateway with `prefix_path: /api` serves.

use std::sync::Arc;

use axum::Router;
use axum::routing::any;
use http::StatusCode;
use toolkit::api::{OpenApiRegistry, OperationBuilder};

use super::dto::{
    ListResponseDto, PluginRequestDto, PluginResponseDto, PluginSourceDto, RouteRequestDto,
    RouteResponseDto, UpstreamRequestDto, UpstreamResponseDto,
};
use super::handlers::{management, proxy};
use super::state::OagwState;

const TAG: &str = "Outbound API Gateway";

/// Base path of the management API, relative to the hosting gateway.
pub const MANAGEMENT_BASE: &str = "/oagw/v1";

/// Register every OAGW route on `router`.
pub fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    state: Arc<OagwState>,
) -> Router {
    let router = register_upstreams(router, openapi);
    let router = register_routes_api(router, openapi);
    let router = register_plugins(router, openapi);
    let router = register_proxy(router, openapi);
    router.layer(axum::Extension(state))
}

fn register_upstreams(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::post("/oagw/v1/upstreams")
        .operation_id("oagw.create_upstream")
        .summary("Create an upstream")
        .description(
            "Register an external service. The alias is auto-derived from hostname endpoints; \
             IP-based or otherwise non-derivable endpoint pools require an explicit alias.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<UpstreamRequestDto>(openapi, "Upstream configuration")
        .handler(management::create_upstream)
        .json_response_with_schema::<UpstreamResponseDto>(
            openapi,
            StatusCode::CREATED,
            "Upstream created",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/upstreams")
        .operation_id("oagw.list_upstreams")
        .summary("List upstreams")
        .description("List the calling tenant's upstreams. Supports OData query parameters.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .query_param("$filter", false, "OData filter, e.g. `alias eq 'api.openai.com'`")
        .query_param("$select", false, "Comma-separated fields to return")
        .query_param("$orderby", false, "Sort order, e.g. `alias desc`")
        .query_param("$top", false, "Maximum results (default 50, max 100)")
        .query_param("$skip", false, "Offset for pagination")
        .handler(management::list_upstreams)
        .json_response_with_schema::<ListResponseDto>(openapi, StatusCode::OK, "Matching upstreams")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.get_upstream")
        .summary("Get an upstream")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream UUID, or `gts.cf.core.oagw.upstream.v1~{uuid}`")
        .handler(management::get_upstream)
        .json_response_with_schema::<UpstreamResponseDto>(openapi, StatusCode::OK, "The upstream")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::put("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.replace_upstream")
        .summary("Replace an upstream")
        .description(
            "Full replacement: omitted optional fields are cleared. The alias is immutable — \
             an endpoint change that would derive a different alias is rejected.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream UUID, or `gts.cf.core.oagw.upstream.v1~{uuid}`")
        .json_request::<UpstreamRequestDto>(openapi, "Replacement upstream configuration")
        .handler(management::replace_upstream)
        .json_response_with_schema::<UpstreamResponseDto>(
            openapi,
            StatusCode::OK,
            "Upstream replaced",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    OperationBuilder::delete("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.delete_upstream")
        .summary("Delete an upstream")
        .description("Deletes the upstream and cascades to its routes.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream UUID, or `gts.cf.core.oagw.upstream.v1~{uuid}`")
        .handler(management::delete_upstream)
        .no_content_response(StatusCode::NO_CONTENT, "Upstream deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi)
}

fn register_routes_api(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::post("/oagw/v1/routes")
        .operation_id("oagw.create_route")
        .summary("Create a route")
        .description("Attach a match rule to one of the calling tenant's upstreams.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<RouteRequestDto>(openapi, "Route configuration")
        .handler(management::create_route)
        .json_response_with_schema::<RouteResponseDto>(
            openapi,
            StatusCode::CREATED,
            "Route created",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/routes")
        .operation_id("oagw.list_routes")
        .summary("List routes")
        .description("List the calling tenant's routes. Supports OData query parameters.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .query_param("$filter", false, "OData filter, e.g. `upstream_id eq '{uuid}'`")
        .query_param("$select", false, "Comma-separated fields to return")
        .query_param("$orderby", false, "Sort order")
        .query_param("$top", false, "Maximum results (default 50, max 100)")
        .query_param("$skip", false, "Offset for pagination")
        .handler(management::list_routes)
        .json_response_with_schema::<ListResponseDto>(openapi, StatusCode::OK, "Matching routes")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/routes/{id}")
        .operation_id("oagw.get_route")
        .summary("Get a route")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route UUID, or `gts.cf.core.oagw.route.v1~{uuid}`")
        .handler(management::get_route)
        .json_response_with_schema::<RouteResponseDto>(openapi, StatusCode::OK, "The route")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::put("/oagw/v1/routes/{id}")
        .operation_id("oagw.replace_route")
        .summary("Replace a route")
        .description("Full replacement. `upstream_id` is immutable and ignored if supplied.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route UUID, or `gts.cf.core.oagw.route.v1~{uuid}`")
        .json_request::<RouteRequestDto>(openapi, "Replacement route configuration")
        .handler(management::replace_route)
        .json_response_with_schema::<RouteResponseDto>(openapi, StatusCode::OK, "Route replaced")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
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
        .path_param("id", "Route UUID, or `gts.cf.core.oagw.route.v1~{uuid}`")
        .handler(management::delete_route)
        .no_content_response(StatusCode::NO_CONTENT, "Route deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi)
}

fn register_plugins(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.create_plugin")
        .summary("Create a custom plugin")
        .description("Plugin definitions are immutable; publish a new one to change behaviour.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<PluginRequestDto>(openapi, "Plugin definition")
        .handler(management::create_plugin)
        .json_response_with_schema::<PluginResponseDto>(
            openapi,
            StatusCode::CREATED,
            "Plugin created",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/plugins")
        .operation_id("oagw.list_plugins")
        .summary("List custom plugins")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .query_param("$filter", false, "OData filter, e.g. `plugin_type eq 'guard'`")
        .query_param("$select", false, "Comma-separated fields to return")
        .query_param("$top", false, "Maximum results (default 50, max 100)")
        .query_param("$skip", false, "Offset for pagination")
        .handler(management::list_plugins)
        .json_response_with_schema::<ListResponseDto>(openapi, StatusCode::OK, "Matching plugins")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/plugins/{id}")
        .operation_id("oagw.get_plugin")
        .summary("Get a custom plugin")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin UUID, or `gts.cf.core.oagw.{type}_plugin.v1~{uuid}`")
        .handler(management::get_plugin)
        .json_response_with_schema::<PluginResponseDto>(openapi, StatusCode::OK, "The plugin")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/plugins/{id}/source")
        .operation_id("oagw.get_plugin_source")
        .summary("Get a custom plugin's source")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin UUID, or `gts.cf.core.oagw.{type}_plugin.v1~{uuid}`")
        .handler(management::get_plugin_source)
        .json_response_with_schema::<PluginSourceDto>(openapi, StatusCode::OK, "The plugin source")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    OperationBuilder::delete("/oagw/v1/plugins/{id}")
        .operation_id("oagw.delete_plugin")
        .summary("Delete a custom plugin")
        .description("Fails with 409 while the plugin is referenced by an upstream or route.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin UUID, or `gts.cf.core.oagw.{type}_plugin.v1~{uuid}`")
        .handler(management::delete_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "Plugin deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi)
}

/// The proxy endpoint accepts any method, so it is registered with an
/// explicit `any` method router rather than a per-verb handler. The
/// `OperationBuilder` call still publishes the operation so the route shows
/// up in the OpenAPI document and in the gateway's auth policy.
fn register_proxy(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::post("/oagw/v1/proxy/{alias}")
        .operation_id("oagw.proxy_root")
        .summary("Proxy a request to an upstream")
        .description(
            "Forwards the request to the upstream resolved from `{alias}`, injecting \
             credentials and applying the configured plugin chain. Any HTTP method is \
             accepted; SSE responses and protocol upgrades are streamed through.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("alias", "Upstream alias")
        .method_router(any(proxy::proxy_root))
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

    OperationBuilder::post("/oagw/v1/proxy/{alias}/{*path}")
        .operation_id("oagw.proxy_path")
        .summary("Proxy a request to an upstream path")
        .description(
            "As `oagw.proxy_root`, with the trailing path appended to the matched route's \
             path when the route's `path_suffix_mode` is `append`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("alias", "Upstream alias")
        .path_param("path", "Path suffix forwarded to the upstream")
        .method_router(any(proxy::proxy_with_path))
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
        .register(router, openapi)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn management_base_is_gear_relative() {
        assert!(
            !MANAGEMENT_BASE.starts_with("/api"),
            "the api-gateway supplies its own prefix; repeating it here would 404 every route"
        );
        assert_eq!(MANAGEMENT_BASE, "/oagw/v1");
    }
}
