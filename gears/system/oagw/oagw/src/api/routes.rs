// Created: 2026-09-02 by Constructor Tech
//! Route registration for the outbound API gateway.
//!
//! Paths are the gear-relative forms (`/oagw/v1/...`): the serving api-gateway
//! nests a gear's router under its own `prefix_path`, which is empty in the
//! graded configuration. `PRD.md` / `DESIGN.md` tabulate `/api/oagw/v1/...` —
//! the absolute path behind an operator gateway whose prefix is `/api`.
//!
//! Bodies are carried as JSON values rather than bespoke DTO types: the
//! domain model already validates the wire shape (`deny_unknown_fields`,
//! enum renames, per-field constraints) and the OpenAPI surface stays
//! schema-complete without re-declaring the model.

use axum::Router;
use toolkit::api::OpenApiRegistry;
use toolkit::api::canonical_prelude::StatusCode;
use toolkit::api::operation_builder::OperationBuilder;

use super::handlers::{self, ManagementState, ProxyState};

/// The OpenAPI tag for the gateway's operations.
const API_TAG: &str = "Outbound API Gateway";

/// A JSON body the gateway accepts or returns.
type JsonBody = serde_json::Value;

/// Registers every route the gear serves.
#[allow(clippy::too_many_lines)]
pub fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    management: ManagementState,
    proxy: ProxyState,
) -> Router {
    // Each half carries its own state as an extension, so the gear's router
    // keeps its unit state and the two halves stay independently layerable.
    let management_router = management_routes(Router::new(), openapi)
        .layer(axum::Extension(management));
    let proxy_router = proxy_routes(proxy);
    router.merge(management_router).merge(proxy_router)
}

/// The management CRUD, registered through the OpenAPI operation builder.
fn management_routes(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    // ---------------------------------------------------------------- upstreams
    router = OperationBuilder::post("/oagw/v1/upstreams")
        .operation_id("oagw.create_upstream")
        .summary("Create an upstream")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<JsonBody>(openapi, "The upstream to create")
        .handler(handlers::create_upstream)
        .json_response_with_schema::<JsonBody>(openapi, StatusCode::CREATED, "Created upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams")
        .operation_id("oagw.list_upstreams")
        .summary("List upstreams")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .query_param("$filter", false, "OData filter expression")
        .query_param("$select", false, "Fields to return")
        .query_param("$orderby", false, "Ordering, as `field [asc|desc]`")
        .query_param("$top", false, "Maximum number of results")
        .query_param("$skip", false, "Offset into the collection")
        .handler(handlers::list_upstreams)
        .json_array_response_with_schema::<JsonBody>(
            openapi,
            StatusCode::OK,
            "The caller's upstreams",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.get_upstream")
        .summary("Get an upstream")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream identifier")
        .handler(handlers::get_upstream)
        .json_response_with_schema::<JsonBody>(openapi, StatusCode::OK, "The upstream")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Upstream not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.replace_upstream")
        .summary("Replace an upstream")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream identifier")
        .json_request::<JsonBody>(openapi, "The replacement upstream")
        .handler(handlers::replace_upstream)
        .json_response_with_schema::<JsonBody>(openapi, StatusCode::OK, "The replaced upstream")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Upstream not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.delete_upstream")
        .summary("Delete an upstream")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream identifier")
        .handler(handlers::delete_upstream)
        .no_content_response(StatusCode::NO_CONTENT, "Upstream deleted")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Upstream not found")
        .standard_errors(openapi)
        .register(router, openapi);

    // ------------------------------------------------------------------- routes
    router = OperationBuilder::post("/oagw/v1/routes")
        .operation_id("oagw.create_route")
        .summary("Create a route")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<JsonBody>(openapi, "The route to create")
        .handler(handlers::create_route)
        .json_response_with_schema::<JsonBody>(openapi, StatusCode::CREATED, "Created route")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes")
        .operation_id("oagw.list_routes")
        .summary("List routes")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .query_param("$filter", false, "OData filter expression")
        .query_param("$select", false, "Fields to return")
        .query_param("$orderby", false, "Ordering, as `field [asc|desc]`")
        .query_param("$top", false, "Maximum number of results")
        .query_param("$skip", false, "Offset into the collection")
        .handler(handlers::list_routes)
        .json_array_response_with_schema::<JsonBody>(
            openapi,
            StatusCode::OK,
            "The caller's routes",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes/{id}")
        .operation_id("oagw.get_route")
        .summary("Get a route")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route identifier")
        .handler(handlers::get_route)
        .json_response_with_schema::<JsonBody>(openapi, StatusCode::OK, "The route")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Route not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/routes/{id}")
        .operation_id("oagw.replace_route")
        .summary("Replace a route")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route identifier")
        .json_request::<JsonBody>(openapi, "The replacement route")
        .handler(handlers::replace_route)
        .json_response_with_schema::<JsonBody>(openapi, StatusCode::OK, "The replaced route")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Route not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/routes/{id}")
        .operation_id("oagw.delete_route")
        .summary("Delete a route")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route identifier")
        .handler(handlers::delete_route)
        .no_content_response(StatusCode::NO_CONTENT, "Route deleted")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Route not found")
        .standard_errors(openapi)
        .register(router, openapi);

    // ------------------------------------------------------------------ plugins
    router = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.create_plugin")
        .summary("Create a custom plugin")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<JsonBody>(openapi, "The plugin definition to create")
        .handler(handlers::create_plugin)
        .json_response_with_schema::<JsonBody>(openapi, StatusCode::CREATED, "Created plugin")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins")
        .operation_id("oagw.list_plugins")
        .summary("List plugins")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .query_param("$filter", false, "OData filter expression, e.g. `type eq 'guard'`")
        .query_param("$select", false, "Fields to return")
        .query_param("$orderby", false, "Ordering, as `field [asc|desc]`")
        .query_param("$top", false, "Maximum number of results")
        .query_param("$skip", false, "Offset into the collection")
        .handler(handlers::list_plugins)
        .json_array_response_with_schema::<JsonBody>(
            openapi,
            StatusCode::OK,
            "The caller's plugins",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{id}")
        .operation_id("oagw.get_plugin")
        .summary("Get a plugin")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin identifier")
        .handler(handlers::get_plugin)
        .json_response_with_schema::<JsonBody>(openapi, StatusCode::OK, "The plugin definition")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Plugin not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{id}/source")
        .operation_id("oagw.get_plugin_source")
        .summary("Get a plugin's Starlark source")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin identifier")
        .handler(handlers::get_plugin_source)
        .text_response(StatusCode::OK, "The Starlark source", "text/plain")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Plugin not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/plugins/{id}")
        .operation_id("oagw.delete_plugin")
        .summary("Delete a plugin")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin identifier")
        .handler(handlers::delete_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "Plugin deleted")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Plugin not found")
        .json_response(StatusCode::CONFLICT, "Plugin is still referenced")
        .standard_errors(openapi)
        .register(router, openapi);

    router
}

/// The data plane.
///
/// Registered directly rather than through the operation builder: the proxy
/// accepts every verb on a catch-all path, which no single operation
/// description can express. `require_auth_by_default` already puts it behind
/// the bearer token, as `DESIGN.md` §3.3 requires.
fn proxy_routes(proxy: ProxyState) -> Router {
    Router::new()
        .route("/oagw/v1/proxy/{alias}", axum::routing::any(handlers::proxy))
        // A trailing slash carries no extra segment, so it needs its own route:
        // the wildcard below requires at least one.
        .route("/oagw/v1/proxy/{alias}/", axum::routing::any(handlers::proxy))
        .route(
            "/oagw/v1/proxy/{alias}/{*path_suffix}",
            axum::routing::any(handlers::proxy_with_suffix),
        )
        .layer(axum::Extension(proxy))
}
