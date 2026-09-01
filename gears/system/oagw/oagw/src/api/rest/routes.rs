//! REST route registration for the OAGW gear.
//!
//! The management API lives under `/api/oagw/v1/...` (upstreams, routes,
//! plugins) and the proxy API under `/api/oagw/v1/proxy/{alias}[/{suffix}]`.

use std::sync::Arc;

use axum::Router;
use axum::extract::Extension;
use axum::http::Method;
use toolkit::api::OpenApiRegistry;
use toolkit::api::canonical_prelude::StatusCode;
use toolkit::api::operation_builder::{
    CORE_GLOBAL_BASE_LICENSE_FEATURE, LicenseFeature, OperationBuilder,
};

use super::handlers::{self, ControlPlane};
use crate::domain::data_plane::DataPlaneService;

const API_TAG: &str = "OAGW Management";
const PROXY_TAG: &str = "OAGW Proxy";

const UPSTREAMS_PATH: &str = "/api/oagw/v1/upstreams";
const UPSTREAM_PATH: &str = "/api/oagw/v1/upstreams/{id}";
const ROUTES_PATH: &str = "/api/oagw/v1/routes";
const ROUTE_PATH: &str = "/api/oagw/v1/routes/{id}";
const PLUGINS_PATH: &str = "/api/oagw/v1/plugins";
const PLUGIN_PATH: &str = "/api/oagw/v1/plugins/{id}";
const PLUGIN_SOURCE_PATH: &str = "/api/oagw/v1/plugins/{id}/source";
const PROXY_BARE_PATH: &str = "/api/oagw/v1/proxy/{alias}";
const PROXY_SUFFIXED_PATH: &str = "/api/oagw/v1/proxy/{alias}/{*suffix}";

struct License;

impl AsRef<str> for License {
    fn as_ref(&self) -> &'static str {
        CORE_GLOBAL_BASE_LICENSE_FEATURE
    }
}

impl LicenseFeature for License {}

/// Registers all REST routes for the OAGW gear.
#[allow(clippy::needless_pass_by_value)]
pub fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    control_plane: Arc<ControlPlane>,
    data_plane: Arc<DataPlaneService>,
) -> Router {
    let router = register_upstream_routes(router, openapi);
    let router = register_route_routes(router, openapi);
    let router = register_plugin_routes(router, openapi);

    let router = register_proxy_path(PROXY_BARE_PATH, "bare", router, openapi);
    let router = register_proxy_path(PROXY_SUFFIXED_PATH, "suffix", router, openapi);

    router
        .layer(Extension(control_plane))
        .layer(Extension(data_plane))
}

/// Registers the upstream CRUD routes.
#[allow(clippy::needless_pass_by_value)]
fn register_upstream_routes(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    router = OperationBuilder::post(UPSTREAMS_PATH)
        .operation_id("oagw.upstreams.create")
        .summary("Create upstream")
        .description("Create an upstream. The alias is auto-derived from hostname endpoints unless the pool is IP-based or otherwise non-derivable, in which case an explicit alias is required. 409 on alias conflict.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .json_request_schema("UpstreamRequest", "Upstream payload")
        .handler(handlers::create_upstream)
        .json_response(StatusCode::CREATED, "Created upstream")
        .problem_response(openapi, StatusCode::CONFLICT, "Alias already in use")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get(UPSTREAMS_PATH)
        .operation_id("oagw.upstreams.list")
        .summary("List upstreams")
        .description("List upstreams owned by the calling tenant (strict tenant scoping). Supports `$top`/`$skip` pagination.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .query_param("$top", false, "Max results (default 50, max 100)")
        .query_param("$skip", false, "Offset for pagination")
        .handler(handlers::list_upstreams)
        .json_response(StatusCode::OK, "List of upstreams")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get(UPSTREAM_PATH)
        .operation_id("oagw.upstreams.get")
        .summary("Get upstream by ID")
        .description("Get a single upstream by its UUID. Ancestor resources are invisible (404).")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Upstream UUID")
        .handler(handlers::get_upstream)
        .json_response(StatusCode::OK, "The upstream")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Upstream not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::put(UPSTREAM_PATH)
        .operation_id("oagw.upstreams.replace")
        .summary("Replace upstream")
        .description(
            "Full replacement. The alias is immutable and is preserved from the stored entity.",
        )
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Upstream UUID")
        .json_request_schema("UpstreamRequest", "Upstream payload")
        .handler(handlers::replace_upstream)
        .json_response(StatusCode::OK, "Replaced upstream")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Upstream not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete(UPSTREAM_PATH)
        .operation_id("oagw.upstreams.delete")
        .summary("Delete upstream")
        .description(
            "Delete an upstream. Fails with a validation error when routes still reference it.",
        )
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Upstream UUID")
        .handler(handlers::delete_upstream)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Upstream not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router
}

/// Registers the route CRUD routes.
#[allow(clippy::needless_pass_by_value)]
fn register_route_routes(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    router = OperationBuilder::post(ROUTES_PATH)
        .operation_id("oagw.routes.create")
        .summary("Create route")
        .description("Create a route bound to an upstream in the calling tenant scope.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .json_request_schema("RouteRequest", "Route payload")
        .handler(handlers::create_route)
        .json_response(StatusCode::CREATED, "Created route")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get(ROUTES_PATH)
        .operation_id("oagw.routes.list")
        .summary("List routes")
        .description("List routes owned by the calling tenant. Supports `$top`/`$skip` pagination.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .query_param("$top", false, "Max results")
        .query_param("$skip", false, "Offset for pagination")
        .handler(handlers::list_routes)
        .json_response(StatusCode::OK, "List of routes")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get(ROUTE_PATH)
        .operation_id("oagw.routes.get")
        .summary("Get route by ID")
        .description("Get a single route by its UUID.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Route UUID")
        .handler(handlers::get_route)
        .json_response(StatusCode::OK, "The route")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Route not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::put(ROUTE_PATH)
        .operation_id("oagw.routes.replace")
        .summary("Replace route")
        .description("Full replacement of a route (upstream_id is re-validated but immutable).")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Route UUID")
        .json_request_schema("RouteRequest", "Route payload")
        .handler(handlers::replace_route)
        .json_response(StatusCode::OK, "Replaced route")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Route not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete(ROUTE_PATH)
        .operation_id("oagw.routes.delete")
        .summary("Delete route")
        .description("Delete a route by UUID.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Route UUID")
        .handler(handlers::delete_route)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Route not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router
}

/// Registers the custom-plugin routes.
#[allow(clippy::needless_pass_by_value)]
fn register_plugin_routes(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    router = OperationBuilder::post(PLUGINS_PATH)
        .operation_id("oagw.plugins.create")
        .summary("Create plugin")
        .description("Create a custom (Starlark) plugin. Plugins are immutable after creation.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .json_request_schema("PluginRequest", "Plugin payload")
        .handler(handlers::create_plugin)
        .json_response(StatusCode::CREATED, "Created plugin")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get(PLUGINS_PATH)
        .operation_id("oagw.plugins.list")
        .summary("List plugins")
        .description(
            "List custom plugins owned by the calling tenant. Supports `$top`/`$skip` pagination.",
        )
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .query_param("$top", false, "Max results")
        .query_param("$skip", false, "Offset for pagination")
        .handler(handlers::list_plugins)
        .json_response(StatusCode::OK, "List of plugins")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get(PLUGIN_PATH)
        .operation_id("oagw.plugins.get")
        .summary("Get plugin by ID")
        .description("Get a single custom plugin by its UUID.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Plugin UUID")
        .handler(handlers::get_plugin)
        .json_response(StatusCode::OK, "The plugin")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Plugin not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete(PLUGIN_PATH)
        .operation_id("oagw.plugins.delete")
        .summary("Delete plugin")
        .description("Delete a custom plugin. Returns 409 PluginInUse when upstreams or routes still bind it.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Plugin UUID")
        .handler(handlers::delete_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Plugin not found")
        .problem_response(openapi, StatusCode::CONFLICT, "Plugin still in use")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get(PLUGIN_SOURCE_PATH)
        .operation_id("oagw.plugins.get_source")
        .summary("Get plugin source")
        .description("Get the raw Starlark source of a custom plugin.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Plugin UUID")
        .handler(handlers::get_plugin_source)
        .text_response(StatusCode::OK, "Plugin source", "text/plain")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Plugin not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router
}

/// Register every supported proxy method at `path`.
///
/// The bare-alias path (`{alias}`) and the suffixed path
/// (`{alias}/{*suffix}`) are both registered because an axum wildcard
/// requires at least one segment.  Each method routes to the same typeless
/// handler: permission, CORS, routing, plugin execution, buffering and the
/// HTTP/WebSocket upstream call are all decided by [`DataPlaneService`].
fn register_proxy_path(
    path: &str,
    shape: &str,
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
) -> Router {
    for method in [
        Method::GET,
        Method::POST,
        Method::PUT,
        Method::PATCH,
        Method::DELETE,
        Method::OPTIONS,
    ] {
        let method_str = method.as_str().to_ascii_lowercase();
        // Keep operation ids unique across the two path shapes.
        let operation_id = format!("oagw.proxy.{method_str}.{shape}");
        let builder = OperationBuilder::new(method, path)
            .operation_id(operation_id)
            .summary("Proxy request to upstream")
            .description(
                "Resolve the upstream by alias, match a route, run the plugin chain and \
                 forward the request with the credentials injected.",
            )
            .tag(PROXY_TAG)
            .authenticated()
            .require_license_features::<License>([])
            .path_param("alias", "Upstream alias");
        // The proxy response body is streamed / upgraded request-specific;
        // document a generic success + problem response on all methods.
        let response_spec = toolkit::api::operation_builder::ResponseSpec {
            status: 200,
            content_type: "application/json",
            description: "Proxied upstream response (body passthrough)".to_owned(),
            schema: None,
        };
        // Each path shape uses a different Path extractor, so the handler
        // differs — branch on the shape and complete the chain per type.
        router = if shape == "suffix" {
            builder
                .handler(handlers::proxy_suffixed)
                .response(response_spec)
                .error_502(openapi)
                .error_503(openapi)
                .standard_errors(openapi)
                .register(router, openapi)
        } else {
            builder
                .handler(handlers::proxy_bare)
                .response(response_spec)
                .error_502(openapi)
                .error_503(openapi)
                .standard_errors(openapi)
                .register(router, openapi)
        };
    }
    router
}
