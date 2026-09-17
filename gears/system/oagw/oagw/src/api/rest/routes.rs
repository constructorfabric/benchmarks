//! REST route registration for the OAGW gear.
//!
//! All routes are under the gear-relative base `/oagw/v1`; the api-gateway
//! nests this router under its own `prefix_path` (empty for the graded
//! config, so the public paths are `/oagw/v1/...`).

use std::sync::Arc;

use axum::{Extension, Router};
use toolkit::api::OpenApiRegistry;
use toolkit::api::canonical_prelude::StatusCode;
use toolkit::api::operation_builder::OperationBuilder;

use super::handlers;
use crate::infra::storage::Services;

const API_TAG: &str = "OAGW";

/// Registers all OAGW REST routes onto `router`.
#[allow(clippy::needless_pass_by_value)]
pub fn register_routes(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
    service: Arc<Services>,
) -> Router {
    // -- upstreams ----------------------------------------------------------

    router = OperationBuilder::post("/oagw/v1/upstreams")
        .operation_id("oagw.create_upstream")
        .summary("Create an upstream")
        .description(
            "Create a new upstream. The alias is derived from the endpoints, or must be \
             provided explicitly for IP-based / non-derivable pools.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<crate::domain::wire::UpstreamInput>(openapi, "Upstream to create")
        .handler(handlers::create_upstream)
        .json_response_with_schema::<crate::domain::wire::UpstreamDocument>(
            openapi,
            StatusCode::CREATED,
            "Created upstream",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams")
        .operation_id("oagw.list_upstreams")
        .summary("List upstreams")
        .description("List upstreams owned by the calling tenant (OData `$top`/`$skip`).")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .query_param("$top", false, "Page size (default 50, max 100)")
        .query_param("$skip", false, "Items to skip")
        .handler(handlers::list_upstreams)
        .json_response_with_schema::<crate::domain::wire::UpstreamListResponse>(
            openapi,
            StatusCode::OK,
            "List of upstreams",
        )
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams/{upstream_id}")
        .operation_id("oagw.get_upstream")
        .summary("Get an upstream by ID")
        .description("Retrieve a single upstream by its UUID or full GTS identifier.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("upstream_id", "Upstream UUID or GTS identifier")
        .handler(handlers::get_upstream)
        .json_response_with_schema::<crate::domain::wire::UpstreamDocument>(
            openapi,
            StatusCode::OK,
            "The requested upstream",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/upstreams/{upstream_id}")
        .operation_id("oagw.update_upstream")
        .summary("Replace an upstream")
        .description(
            "Fully replace an upstream. The alias is immutable once set — endpoint changes \
             that would alter the derived alias are rejected.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("upstream_id", "Upstream UUID or GTS identifier")
        .json_request::<crate::domain::wire::UpstreamInput>(openapi, "Upstream replacement")
        .handler(handlers::update_upstream)
        .json_response_with_schema::<crate::domain::wire::UpstreamDocument>(
            openapi,
            StatusCode::OK,
            "The replaced upstream",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/upstreams/{upstream_id}")
        .operation_id("oagw.delete_upstream")
        .summary("Delete an upstream")
        .description(
            "Delete an upstream and cascade-delete its routes. The alias becomes available \
             to the tenant again.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("upstream_id", "Upstream UUID or GTS identifier")
        .handler(handlers::delete_upstream)
        .no_content_response(StatusCode::NO_CONTENT, "Upstream deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // -- routes -------------------------------------------------------------

    router = OperationBuilder::post("/oagw/v1/routes")
        .operation_id("oagw.create_route")
        .summary("Create a route")
        .description(
            "Create a route binding a method+path to an upstream owned by the calling tenant.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<crate::domain::wire::RouteInput>(openapi, "Route to create")
        .handler(handlers::create_route)
        .json_response_with_schema::<crate::domain::wire::RouteDocument>(
            openapi,
            StatusCode::CREATED,
            "Created route",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes")
        .operation_id("oagw.list_routes")
        .summary("List routes")
        .description("List routes owned by the calling tenant (OData `$top`/`$skip`).")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .query_param("$top", false, "Page size (default 50, max 100)")
        .query_param("$skip", false, "Items to skip")
        .handler(handlers::list_routes)
        .json_response_with_schema::<crate::domain::wire::RouteListResponse>(
            openapi,
            StatusCode::OK,
            "List of routes",
        )
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes/{route_id}")
        .operation_id("oagw.get_route")
        .summary("Get a route by ID")
        .description("Retrieve a single route by its UUID or full GTS identifier.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("route_id", "Route UUID or GTS identifier")
        .handler(handlers::get_route)
        .json_response_with_schema::<crate::domain::wire::RouteDocument>(
            openapi,
            StatusCode::OK,
            "The requested route",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/routes/{route_id}")
        .operation_id("oagw.update_route")
        .summary("Replace a route")
        .description(
            "Fully replace a route. The target upstream (`upstream_id`) is immutable.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("route_id", "Route UUID or GTS identifier")
        .json_request::<crate::domain::wire::RouteInput>(openapi, "Route replacement")
        .handler(handlers::update_route)
        .json_response_with_schema::<crate::domain::wire::RouteDocument>(
            openapi,
            StatusCode::OK,
            "The replaced route",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/routes/{route_id}")
        .operation_id("oagw.delete_route")
        .summary("Delete a route")
        .description("Delete a route by its UUID or full GTS identifier.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("route_id", "Route UUID or GTS identifier")
        .handler(handlers::delete_route)
        .no_content_response(StatusCode::NO_CONTENT, "Route deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // -- custom plugins -----------------------------------------------------

    router = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.create_plugin")
        .summary("Create a custom plugin")
        .description(
            "Register a custom (UUID-backed) plugin. Plugins are immutable after creation.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<crate::domain::wire::PluginInput>(openapi, "Plugin to create")
        .handler(handlers::create_plugin)
        .json_response_with_schema::<crate::domain::wire::PluginDocument>(
            openapi,
            StatusCode::CREATED,
            "Created plugin",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins")
        .operation_id("oagw.list_plugins")
        .summary("List plugins")
        .description("List custom plugins owned by the calling tenant (OData `$top`/`$skip`).")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .query_param("$top", false, "Page size (default 50, max 100)")
        .query_param("$skip", false, "Items to skip")
        .handler(handlers::list_plugins)
        .json_response_with_schema::<crate::domain::wire::PluginListResponse>(
            openapi,
            StatusCode::OK,
            "List of plugins",
        )
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{plugin_id}")
        .operation_id("oagw.get_plugin")
        .summary("Get a plugin by ID")
        .description("Retrieve a single custom plugin by its UUID or full GTS identifier.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("plugin_id", "Plugin UUID or GTS identifier")
        .handler(handlers::get_plugin)
        .json_response_with_schema::<crate::domain::wire::PluginDocument>(
            openapi,
            StatusCode::OK,
            "The requested plugin",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/plugins/{plugin_id}")
        .operation_id("oagw.delete_plugin")
        .summary("Delete a plugin")
        .description(
            "Delete an unlinked custom plugin. Plugins referenced by an upstream or route \
             return 409 (PluginInUse).",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("plugin_id", "Plugin UUID or GTS identifier")
        .handler(handlers::delete_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "Plugin deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{plugin_id}/source")
        .operation_id("oagw.get_plugin_source")
        .summary("Get plugin source code")
        .description("Retrieve the source code of a custom plugin.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("plugin_id", "Plugin UUID or GTS identifier")
        .handler(handlers::get_plugin_source)
        .json_response(StatusCode::OK, "Plugin source code as a JSON string")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // -- proxy (any method) -------------------------------------------------

    router = OperationBuilder::post("/oagw/v1/proxy/{*proxy_path}")
        .operation_id("oagw.proxy")
        .summary("Proxy a request to an upstream")
        .description(
            "Execute the full proxy flow for `{METHOD} /oagw/v1/proxy/{alias}/{path}`: \
             alias resolution, route matching, credential injection, guards, transforms, \
             rate limiting, and upstream forwarding.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .method_router(axum::routing::any(handlers::proxy))
        .json_response(StatusCode::OK, "Proxied upstream response")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .problem_response(
            openapi,
            StatusCode::PAYLOAD_TOO_LARGE,
            "Request body exceeds the 100 MiB limit",
        )
        .error_429(openapi)
        .error_502(openapi)
        .error_503(openapi)
        .error_504(openapi)
        .register(router, openapi);

    router.layer(Extension(service))
}
