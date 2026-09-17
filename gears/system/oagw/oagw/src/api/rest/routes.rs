//! Route registration for the OAGW REST surface.
//!
//! Every path is registered directly at `/oagw/v1/...` (the host router is
//! used as-is; gear routes are not automatically prefixed). Management
//! routes go through the `OperationBuilder` (openapi-documented,
//! authenticated). The data-plane proxy and its CORS preflight are raw
//! axum routes because they accept *any* HTTP method.

use std::sync::Arc;

use axum::extract::DefaultBodyLimit;
use axum::http::StatusCode;
use axum::routing::MethodRouter;
use axum::{Extension, Router};
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::{CORE_GLOBAL_BASE_LICENSE_FEATURE, LicenseFeature, OperationBuilder};

use super::handlers;
use crate::domain::services::management::ControlPlaneServiceImpl;
use crate::infra::proxy::service::DataPlaneServiceImpl;

const API_TAG: &str = "OAGW";

/// License feature required by the OAGW gear (base platform feature).
struct License;

impl AsRef<str> for License {
    fn as_ref(&self) -> &'static str {
        CORE_GLOBAL_BASE_LICENSE_FEATURE
    }
}

impl LicenseFeature for License {}

/// Register all OAGW routes onto `router`.
#[allow(clippy::needless_pass_by_value)]
pub fn register_routes(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
    control: Arc<ControlPlaneServiceImpl>,
    data: Arc<DataPlaneServiceImpl>,
) -> Router {
    // --- Upstreams ---------------------------------------------------------
    router = OperationBuilder::post("/oagw/v1/upstreams")
        .operation_id("oagw.create_upstream")
        .summary("Create upstream")
        .description("Create an outbound API gateway upstream (server, protocol, auth, headers, plugins, rate limits, CORS).")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .json_request_schema("UpstreamRequest", "Upstream configuration")
        .handler(handlers::create_upstream)
        .json_response(StatusCode::CREATED, "Upstream created")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams")
        .operation_id("oagw.list_upstreams")
        .summary("List upstreams")
        .description("List the calling tenant's upstreams.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .handler(handlers::list_upstreams)
        .json_response(StatusCode::OK, "List of upstreams")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.get_upstream")
        .summary("Get upstream")
        .description("Fetch one upstream by id (UUID or GTS instance id).")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Upstream id (UUID or GTS instance id)")
        .handler(handlers::get_upstream)
        .json_response(StatusCode::OK, "The upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.update_upstream")
        .summary("Replace upstream")
        .description("Replace an upstream. The alias is immutable once set.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Upstream id (UUID or GTS instance id)")
        .json_request_schema("UpstreamRequest", "Upstream configuration")
        .handler(handlers::update_upstream)
        .json_response(StatusCode::OK, "Updated upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.delete_upstream")
        .summary("Delete upstream")
        .description("Delete an upstream; tenant routes bound to it are cascaded and removed.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Upstream id (UUID or GTS instance id)")
        .handler(handlers::delete_upstream)
        .no_content_response(StatusCode::NO_CONTENT, "Upstream deleted")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/oagw/v1/upstreams/{id}/enable")
        .operation_id("oagw.enable_upstream")
        .summary("Enable upstream")
        .description("Mark an upstream enabled so it participates in alias resolution.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Upstream id (UUID or GTS instance id)")
        .handler(handlers::enable_upstream)
        .json_response(StatusCode::OK, "Enabled upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/oagw/v1/upstreams/{id}/disable")
        .operation_id("oagw.disable_upstream")
        .summary("Disable upstream")
        .description("Mark an upstream disabled; proxying to it yields 503 LinkUnavailable.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Upstream id (UUID or GTS instance id)")
        .handler(handlers::disable_upstream)
        .json_response(StatusCode::OK, "Disabled upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    // --- Routes ------------------------------------------------------------
    router = OperationBuilder::post("/oagw/v1/routes")
        .operation_id("oagw.create_route")
        .summary("Create route")
        .description("Create a route bound to an upstream (method allowlist + longest path-prefix matching).")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .json_request_schema("RouteRequest", "Route configuration")
        .handler(handlers::create_route)
        .json_response(StatusCode::CREATED, "Route created")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes")
        .operation_id("oagw.list_routes")
        .summary("List routes")
        .description("List the calling tenant's routes.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .handler(handlers::list_routes)
        .json_response(StatusCode::OK, "List of routes")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes/{id}")
        .operation_id("oagw.get_route")
        .summary("Get route")
        .description("Fetch one route by id (UUID or GTS instance id).")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Route id (UUID or GTS instance id)")
        .handler(handlers::get_route)
        .json_response(StatusCode::OK, "The route")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/routes/{id}")
        .operation_id("oagw.update_route")
        .summary("Replace route")
        .description("Replace a route. upstream_id is immutable after creation.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Route id (UUID or GTS instance id)")
        .json_request_schema("RouteRequest", "Route configuration")
        .handler(handlers::update_route)
        .json_response(StatusCode::OK, "Updated route")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/routes/{id}")
        .operation_id("oagw.delete_route")
        .summary("Delete route")
        .description("Delete a route.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Route id (UUID or GTS instance id)")
        .handler(handlers::delete_route)
        .no_content_response(StatusCode::NO_CONTENT, "Route deleted")
        .standard_errors(openapi)
        .register(router, openapi);

    // --- Custom plugins -----------------------------------------------------
    router = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.create_plugin")
        .summary("Create plugin")
        .description("Create a custom Starlark plugin (auth/guard/transform).")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .json_request_schema("PluginRequest", "Plugin configuration")
        .handler(handlers::create_plugin)
        .json_response(StatusCode::CREATED, "Plugin created")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins")
        .operation_id("oagw.list_plugins")
        .summary("List plugins")
        .description("List the calling tenant's custom plugins.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .handler(handlers::list_plugins)
        .json_response(StatusCode::OK, "List of plugins")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{id}")
        .operation_id("oagw.get_plugin")
        .summary("Get plugin")
        .description("Fetch one custom plugin by id (UUID or GTS instance id).")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Plugin id (UUID or GTS instance id)")
        .handler(handlers::get_plugin)
        .json_response(StatusCode::OK, "The plugin")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/plugins/{id}")
        .operation_id("oagw.delete_plugin")
        .summary("Delete plugin")
        .description("Delete a custom plugin. Returns 409 PluginInUse while referenced.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Plugin id (UUID or GTS instance id)")
        .handler(handlers::delete_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "Plugin deleted")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{id}/source")
        .operation_id("oagw.get_plugin_source")
        .summary("Get plugin source")
        .description("Fetch the Starlark source of a custom plugin.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Plugin id (UUID or GTS instance id)")
        .handler(handlers::get_plugin_source)
        .json_response(StatusCode::OK, "The plugin source")
        .standard_errors(openapi)
        .register(router, openapi);

    // --- Data plane ----------------------------------------------------------
    // Raw routes: accept any proxy method; CORS preflight (OPTIONS) is
    // answered permissively at the handler level (ADR 0004).
    let proxy = MethodRouter::new()
        .get(handlers::proxy_no_suffix)
        .post(handlers::proxy_no_suffix)
        .put(handlers::proxy_no_suffix)
        .patch(handlers::proxy_no_suffix)
        .delete(handlers::proxy_no_suffix)
        .head(handlers::proxy_no_suffix);
    // NOTE: the suffix route needs `proxy_with_suffix` (Path<(String, String)>);
    // reusing `proxy.clone()` (Path<String>) makes axum reject every
    // request with "Wrong number of path arguments for `Path`. Expected 1
    // but got 2".
    let proxy_with_suffix = MethodRouter::new()
        .get(handlers::proxy_with_suffix)
        .post(handlers::proxy_with_suffix)
        .put(handlers::proxy_with_suffix)
        .patch(handlers::proxy_with_suffix)
        .delete(handlers::proxy_with_suffix)
        .head(handlers::proxy_with_suffix);
    let preflight = MethodRouter::new().options(handlers::proxy_preflight);

    router = router
        .route("/oagw/v1/proxy/{alias}", proxy)
        .route("/oagw/v1/proxy/{alias}/{*path_suffix}", proxy_with_suffix)
        .route("/oagw/v1/proxy/{alias}", preflight.clone())
        .route("/oagw/v1/proxy/{alias}/{*path_suffix}", preflight);

    // Cap the *control-plane* request body; the proxy enforces its own
    // `body_limit_bytes` (413) inside the data plane.
    router = router.layer(DefaultBodyLimit::max(16 * 1024 * 1024)).layer(Extension(control)).layer(Extension(data));

    router
}
