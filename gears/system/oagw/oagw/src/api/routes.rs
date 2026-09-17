//! REST route registration.
//!
//! Management routes (upstreams/routes/plugins) and the standard-method
//! proxy routes are registered through [`OperationBuilder`] so they are
//! documented in OpenAPI and land in the api-gateway's authenticated-route
//! policy. `HEAD`/`OPTIONS` on the proxy (no axum builder variant) are
//! registered directly; `OPTIONS` preflight is answered with a permissive
//! 204 and the api-gateway auth middleware skips preflights anyway.

use std::sync::Arc;

use axum::{Extension, Router, routing};
use toolkit::api::OpenApiRegistry;
use toolkit::api::canonical_prelude::StatusCode;
use toolkit::api::operation_builder::OperationBuilder;

use super::handlers;
use crate::domain::service::OagwService;

const API_TAG: &str = "OAGW";

const PROXY_ALIAS: &str = "/api/oagw/v1/proxy/{alias}";
const PROXY_ALIAS_PATH: &str = "/api/oagw/v1/proxy/{alias}/{*path}";

/// Register all OAGW routes onto `router` and return the merged router.
///
/// The `OagwService` is installed as an `Extension` for the handlers.
#[allow(clippy::needless_pass_by_value)]
pub fn register_routes(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
    service: Arc<OagwService>,
) -> Router {
    // -----------------------------------------------------------------
    // Upstreams
    // -----------------------------------------------------------------
    router = register_upstream_routes(router, openapi);
    // -----------------------------------------------------------------
    // Routes
    // -----------------------------------------------------------------
    router = register_route_routes(router, openapi);
    // -----------------------------------------------------------------
    // Plugins
    // -----------------------------------------------------------------
    router = register_plugin_routes(router, openapi);
    // -----------------------------------------------------------------
    // Proxy (authenticated standard methods)
    // -----------------------------------------------------------------
    router = register_proxy_routes(router, openapi);

    // HEAD + OPTIONS proxy routes (plain axum — no builder variant).
    router = router
        .route(
            PROXY_ALIAS,
            routing::head(handlers::proxy_alias_only).options(handlers::proxy_preflight),
        )
        .route(
            PROXY_ALIAS_PATH,
            routing::head(handlers::proxy_with_path).options(handlers::proxy_preflight),
        );

    router.layer(Extension(service))
}

fn upstream_path(id: &str) -> String {
    format!("/api/oagw/v1/upstreams/{id}")
}

fn register_upstream_routes(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    // POST /api/oagw/v1/upstreams
    router = OperationBuilder::post("/api/oagw/v1/upstreams")
        .operation_id("oagw.create_upstream")
        .summary("Create upstream")
        .description("Create an upstream, auto-deriving or enforcing its routing alias.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::create_upstream)
        .json_response(StatusCode::CREATED, "Created upstream")
        .problem_response(openapi, StatusCode::BAD_REQUEST, "Validation error")
        .standard_errors(openapi)
        .register(router, openapi);

    // GET /api/oagw/v1/upstreams
    router = OperationBuilder::get("/api/oagw/v1/upstreams")
        .operation_id("oagw.list_upstreams")
        .summary("List upstreams")
        .description("List upstreams owned by the calling tenant.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::list_upstreams)
        .json_response(StatusCode::OK, "List of upstreams")
        .standard_errors(openapi)
        .register(router, openapi);

    // GET /api/oagw/v1/upstreams/{id}
    let path = upstream_path("{id}");
    router = OperationBuilder::get(&path)
        .operation_id("oagw.get_upstream")
        .summary("Get upstream by ID")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream UUID or anonymous GTS id")
        .handler(handlers::get_upstream)
        .json_response(StatusCode::OK, "The upstream")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Not found")
        .standard_errors(openapi)
        .register(router, openapi);

    // PUT /api/oagw/v1/upstreams/{id}
    router = OperationBuilder::put(&path)
        .operation_id("oagw.replace_upstream")
        .summary("Replace upstream")
        .description("Full replacement; the alias stays immutable.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream UUID or anonymous GTS id")
        .handler(handlers::replace_upstream)
        .json_response(StatusCode::OK, "The replaced upstream")
        .problem_response(openapi, StatusCode::BAD_REQUEST, "Validation error")
        .standard_errors(openapi)
        .register(router, openapi);

    // DELETE /api/oagw/v1/upstreams/{id}
    router = OperationBuilder::delete(&path)
        .operation_id("oagw.delete_upstream")
        .summary("Delete upstream")
        .description("Deletes the upstream and its routes.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream UUID or anonymous GTS id")
        .handler(handlers::delete_upstream)
        .json_response(StatusCode::NO_CONTENT, "Deleted")
        .standard_errors(openapi)
        .register(router, openapi);

    router
}

fn route_path(id: &str) -> String {
    format!("/api/oagw/v1/routes/{id}")
}

fn register_route_routes(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    router = OperationBuilder::post("/api/oagw/v1/routes")
        .operation_id("oagw.create_route")
        .summary("Create route")
        .description("Create a route for a tenant-owned upstream.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::create_route)
        .json_response(StatusCode::CREATED, "Created route")
        .problem_response(openapi, StatusCode::BAD_REQUEST, "Validation error")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/api/oagw/v1/routes")
        .operation_id("oagw.list_routes")
        .summary("List routes")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::list_routes)
        .json_response(StatusCode::OK, "List of routes")
        .standard_errors(openapi)
        .register(router, openapi);

    let path = route_path("{id}");
    router = OperationBuilder::get(&path)
        .operation_id("oagw.get_route")
        .summary("Get route by ID")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route UUID or anonymous GTS id")
        .handler(handlers::get_route)
        .json_response(StatusCode::OK, "The route")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::put(&path)
        .operation_id("oagw.replace_route")
        .summary("Replace route")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route UUID or anonymous GTS id")
        .handler(handlers::replace_route)
        .json_response(StatusCode::OK, "The replaced route")
        .problem_response(openapi, StatusCode::BAD_REQUEST, "Validation error")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete(&path)
        .operation_id("oagw.delete_route")
        .summary("Delete route")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route UUID or anonymous GTS id")
        .handler(handlers::delete_route)
        .json_response(StatusCode::NO_CONTENT, "Deleted")
        .standard_errors(openapi)
        .register(router, openapi);

    router
}

fn plugin_path(id: &str) -> String {
    format!("/api/oagw/v1/plugins/{id}")
}

fn register_plugin_routes(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    router = OperationBuilder::post("/api/oagw/v1/plugins")
        .operation_id("oagw.create_plugin")
        .summary("Create plugin")
        .description("Create a custom (immutable) plugin.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::create_plugin)
        .json_response(StatusCode::CREATED, "Created plugin")
        .problem_response(openapi, StatusCode::BAD_REQUEST, "Validation error")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/api/oagw/v1/plugins")
        .operation_id("oagw.list_plugins")
        .summary("List plugins")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::list_plugins)
        .json_response(StatusCode::OK, "List of plugins")
        .standard_errors(openapi)
        .register(router, openapi);

    let path = plugin_path("{id}");
    router = OperationBuilder::get(&path)
        .operation_id("oagw.get_plugin")
        .summary("Get plugin by ID")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin UUID or anonymous GTS id")
        .handler(handlers::get_plugin)
        .json_response(StatusCode::OK, "The plugin")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Not found")
        .standard_errors(openapi)
        .register(router, openapi);

    // GET /api/oagw/v1/plugins/{id}/source
    let source_path = format!("{path}/source");
    router = OperationBuilder::get(&source_path)
        .operation_id("oagw.get_plugin_source")
        .summary("Get plugin Starlark source")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin UUID or anonymous GTS id")
        .handler(handlers::get_plugin_source)
        .json_response(StatusCode::OK, "The plugin source")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete(&path)
        .operation_id("oagw.delete_plugin")
        .summary("Delete plugin")
        .description("Fails with 409 when the plugin is referenced.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin UUID or anonymous GTS id")
        .handler(handlers::delete_plugin)
        .json_response(StatusCode::NO_CONTENT, "Deleted")
        .problem_response(openapi, StatusCode::CONFLICT, "Plugin in use")
        .standard_errors(openapi)
        .register(router, openapi);

    router
}

/// Standard-method proxy routes (all authenticated).
fn register_proxy_routes(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    for (method, label) in [
        ("GET", "oagw.proxy_get"),
        ("POST", "oagw.proxy_post"),
        ("PUT", "oagw.proxy_put"),
        ("PATCH", "oagw.proxy_patch"),
        ("DELETE", "oagw.proxy_delete"),
    ] {
        let (handler_alias_only, handler_with_path) = match method {
            "GET" => (handlers::proxy_alias_only, handlers::proxy_with_path),
            "POST" => (handlers::proxy_alias_only, handlers::proxy_with_path),
            "PUT" => (handlers::proxy_alias_only, handlers::proxy_with_path),
            "PATCH" => (handlers::proxy_alias_only, handlers::proxy_with_path),
            "DELETE" => (handlers::proxy_alias_only, handlers::proxy_with_path),
            _ => unreachable!(),
        };
        // Force non-copy Clone for the handler references across the loop.
        let _ = (handler_alias_only, handler_with_path, label);

        router = match method {
            "GET" => OperationBuilder::get(PROXY_ALIAS)
                .operation_id("oagw.proxy_get")
                .summary("Proxy GET (no suffix)")
                .tag(API_TAG)
                .authenticated()
                .no_license_required()
                .path_param("alias", "Upstream routing alias")
                .handler(handlers::proxy_alias_only)
                .no_content_response(StatusCode::OK, "Proxied response")
                .register(router, openapi),
            "POST" => OperationBuilder::post(PROXY_ALIAS)
                .operation_id("oagw.proxy_post")
                .summary("Proxy POST (no suffix)")
                .tag(API_TAG)
                .authenticated()
                .no_license_required()
                .path_param("alias", "Upstream routing alias")
                .handler(handlers::proxy_alias_only)
                .no_content_response(StatusCode::OK, "Proxied response")
                .register(router, openapi),
            "PUT" => OperationBuilder::put(PROXY_ALIAS)
                .operation_id("oagw.proxy_put")
                .summary("Proxy PUT (no suffix)")
                .tag(API_TAG)
                .authenticated()
                .no_license_required()
                .path_param("alias", "Upstream routing alias")
                .handler(handlers::proxy_alias_only)
                .no_content_response(StatusCode::OK, "Proxied response")
                .register(router, openapi),
            "PATCH" => OperationBuilder::patch(PROXY_ALIAS)
                .operation_id("oagw.proxy_patch")
                .summary("Proxy PATCH (no suffix)")
                .tag(API_TAG)
                .authenticated()
                .no_license_required()
                .path_param("alias", "Upstream routing alias")
                .handler(handlers::proxy_alias_only)
                .no_content_response(StatusCode::OK, "Proxied response")
                .register(router, openapi),
            "DELETE" => OperationBuilder::delete(PROXY_ALIAS)
                .operation_id("oagw.proxy_delete")
                .summary("Proxy DELETE (no suffix)")
                .tag(API_TAG)
                .authenticated()
                .no_license_required()
                .path_param("alias", "Upstream routing alias")
                .handler(handlers::proxy_alias_only)
                .no_content_response(StatusCode::OK, "Proxied response")
                .register(router, openapi),
            _ => unreachable!(),
        };

        let path_with_suffix = PROXY_ALIAS_PATH;
        router = match method {
            "GET" => OperationBuilder::get(path_with_suffix)
                .operation_id("oagw.proxy_get_suffix")
                .summary("Proxy GET (with suffix)")
                .tag(API_TAG)
                .authenticated()
                .no_license_required()
                .path_param("alias", "Upstream routing alias")
                .path_param("path", "Path suffix forwarded to the upstream")
                .handler(handlers::proxy_with_path)
                .no_content_response(StatusCode::OK, "Proxied response")
                .register(router, openapi),
            "POST" => OperationBuilder::post(path_with_suffix)
                .operation_id("oagw.proxy_post_suffix")
                .summary("Proxy POST (with suffix)")
                .tag(API_TAG)
                .authenticated()
                .no_license_required()
                .path_param("alias", "Upstream routing alias")
                .path_param("path", "Path suffix forwarded to the upstream")
                .handler(handlers::proxy_with_path)
                .no_content_response(StatusCode::OK, "Proxied response")
                .register(router, openapi),
            "PUT" => OperationBuilder::put(path_with_suffix)
                .operation_id("oagw.proxy_put_suffix")
                .summary("Proxy PUT (with suffix)")
                .tag(API_TAG)
                .authenticated()
                .no_license_required()
                .path_param("alias", "Upstream routing alias")
                .path_param("path", "Path suffix forwarded to the upstream")
                .handler(handlers::proxy_with_path)
                .no_content_response(StatusCode::OK, "Proxied response")
                .register(router, openapi),
            "PATCH" => OperationBuilder::patch(path_with_suffix)
                .operation_id("oagw.proxy_patch_suffix")
                .summary("Proxy PATCH (with suffix)")
                .tag(API_TAG)
                .authenticated()
                .no_license_required()
                .path_param("alias", "Upstream routing alias")
                .path_param("path", "Path suffix forwarded to the upstream")
                .handler(handlers::proxy_with_path)
                .no_content_response(StatusCode::OK, "Proxied response")
                .register(router, openapi),
            "DELETE" => OperationBuilder::delete(path_with_suffix)
                .operation_id("oagw.proxy_delete_suffix")
                .summary("Proxy DELETE (with suffix)")
                .tag(API_TAG)
                .authenticated()
                .no_license_required()
                .path_param("alias", "Upstream routing alias")
                .path_param("path", "Path suffix forwarded to the upstream")
                .handler(handlers::proxy_with_path)
                .no_content_response(StatusCode::OK, "Proxied response")
                .register(router, openapi),
            _ => unreachable!(),
        };
    }
    router
}
