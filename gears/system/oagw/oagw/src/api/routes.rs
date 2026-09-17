// Created: 2026-09-04 by Constructor Tech
//! Route registration of the `/oagw/v1` management surface.
//!
//! Every operation is registered through `toolkit::api::OperationBuilder` at a
//! *gear-relative* path (no `/api` segment: the api-gateway nests gear paths
//! under its own prefix, which is empty in the graded configuration), declares
//! its request/response schemas and its error responses so the operation shows
//! up in the served OpenAPI document, and is authenticated but exempt from
//! licensing (`docs/DESIGN.md` §3.3).

use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use toolkit::api::state::{AuthNotSet, LicenseNotSet, Missing};
use toolkit::api::{OpenApiRegistry, OperationBuilder};

use crate::api::dto::{
    CreateRouteRequestDto, PluginResponseDto, PluginSourceResponseDto, RegisterPluginRequestDto,
    ReplaceRouteRequestDto, RouteResponseDto, UpstreamRequestDto, UpstreamResponseDto,
};
use crate::api::handlers;
use crate::controlplane::service::ControlPlaneService;

/// OpenAPI tag of the management surface.
const TAG: &str = "OAGW Management";

/// Registers every management route on `router`, publishing them in
/// `openapi`.
pub fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    svc: Arc<ControlPlaneService>,
) -> Router {
    let router = register_upstream_ops(router, openapi);
    let router = register_route_ops(router, openapi);
    let router = register_plugin_ops(router, openapi);
    router.layer(axum::Extension(svc))
}

/// Registers the upstream operations
/// (`docs/DESIGN.md` §3.3 "Management API").
fn register_upstream_ops(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let list = list_operation(
        OperationBuilder::get("/oagw/v1/upstreams")
            .operation_id("oagw.list_upstreams")
            .summary("List upstreams")
            .description("List the upstreams of the calling tenant, in registration order."),
    );
    let router = OperationBuilder::post("/oagw/v1/upstreams")
        .operation_id("oagw.create_upstream")
        .summary("Create an upstream")
        .description(
            "Register an upstream service. The alias is derived from a hostname-based \
             endpoint pool, or must be supplied for IP-based pools; it is unique per tenant.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<UpstreamRequestDto>(openapi, "Upstream configuration")
        .handler(handlers::create_upstream)
        .json_response_with_schema::<UpstreamResponseDto>(
            openapi,
            StatusCode::CREATED,
            "Upstream created (see Location header)",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = list
        .authenticated()
        .no_license_required()
        .handler(handlers::list_upstreams)
        .json_array_response_with_schema::<UpstreamResponseDto>(
            openapi,
            StatusCode::OK,
            "Upstreams of the tenant",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.get_upstream")
        .summary("Read an upstream")
        .description("Read one upstream of the calling tenant by id.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream identifier")
        .handler(handlers::get_upstream)
        .json_response_with_schema::<UpstreamResponseDto>(
            openapi,
            StatusCode::OK,
            "Upstream configuration",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::put("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.replace_upstream")
        .summary("Replace an upstream")
        .description(
            "Replace an upstream of the calling tenant. A full replacement: omitted \
             optional fields are cleared and the alias is recomputed from the endpoints.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream identifier")
        .json_request::<UpstreamRequestDto>(openapi, "Upstream configuration")
        .handler(handlers::replace_upstream)
        .json_response_with_schema::<UpstreamResponseDto>(
            openapi,
            StatusCode::OK,
            "Replaced upstream configuration",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    OperationBuilder::delete("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.delete_upstream")
        .summary("Delete an upstream")
        .description(
            "Delete an upstream of the calling tenant. Rejects the deletion while a route \
             still points at the upstream.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream identifier")
        .handler(handlers::delete_upstream)
        .no_content_response(StatusCode::NO_CONTENT, "Upstream deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi)
}

/// Registers the route operations
/// (`docs/DESIGN.md` §3.3 "Management API").
fn register_route_ops(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let list = list_operation(
        OperationBuilder::get("/oagw/v1/routes")
            .operation_id("oagw.list_routes")
            .summary("List routes")
            .description("List the routes of the calling tenant, in registration order."),
    );
    let router = OperationBuilder::post("/oagw/v1/routes")
        .operation_id("oagw.create_route")
        .summary("Create a route")
        .description(
            "Bind an inbound prefix (or gRPC method) to an upstream of the calling tenant. \
             The match rule must be unique within the upstream.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<CreateRouteRequestDto>(openapi, "Route configuration")
        .handler(handlers::create_route)
        .json_response_with_schema::<RouteResponseDto>(
            openapi,
            StatusCode::CREATED,
            "Route created (see Location header)",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = list
        .authenticated()
        .no_license_required()
        .handler(handlers::list_routes)
        .json_array_response_with_schema::<RouteResponseDto>(
            openapi,
            StatusCode::OK,
            "Routes of the tenant",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/routes/{id}")
        .operation_id("oagw.get_route")
        .summary("Read a route")
        .description("Read one route of the calling tenant by id.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route identifier")
        .handler(handlers::get_route)
        .json_response_with_schema::<RouteResponseDto>(
            openapi,
            StatusCode::OK,
            "Route configuration",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::put("/oagw/v1/routes/{id}")
        .operation_id("oagw.replace_route")
        .summary("Replace a route")
        .description(
            "Replace a route of the calling tenant. A full replacement; `upstream_id` is \
             immutable and the match rule must stay unique within the upstream.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route identifier")
        .json_request::<ReplaceRouteRequestDto>(openapi, "Route configuration without upstream_id")
        .handler(handlers::replace_route)
        .json_response_with_schema::<RouteResponseDto>(
            openapi,
            StatusCode::OK,
            "Replaced route configuration",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    OperationBuilder::delete("/oagw/v1/routes/{id}")
        .operation_id("oagw.delete_route")
        .summary("Delete a route")
        .description("Delete one route of the calling tenant by id.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route identifier")
        .handler(handlers::delete_route)
        .no_content_response(StatusCode::NO_CONTENT, "Route deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi)
}

/// Registers the plugin operations
/// (`docs/DESIGN.md` §3.3 "Management API").
fn register_plugin_ops(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let list = list_operation(
        OperationBuilder::get("/oagw/v1/plugins")
            .operation_id("oagw.list_plugins")
            .summary("List plugins")
            .description(
                "List the built-in plugin catalog followed by the custom plugins registered \
                 by the calling tenant.",
            ),
    );
    let router = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.register_plugin")
        .summary("Register a custom plugin")
        .description(
            "Register an immutable custom (Starlark) plugin. The name is unique per tenant; \
             the plugin is referenced by its GTS instance id from `plugins.items[]`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<RegisterPluginRequestDto>(openapi, "Plugin kind, name and Starlark source")
        .handler(handlers::register_plugin)
        .json_response_with_schema::<PluginResponseDto>(
            openapi,
            StatusCode::CREATED,
            "Plugin registered (see Location header)",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = list
        .authenticated()
        .no_license_required()
        .handler(handlers::list_plugins)
        .json_array_response_with_schema::<PluginResponseDto>(
            openapi,
            StatusCode::OK,
            "Built-in and custom plugins",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/plugins/{id}")
        .operation_id("oagw.get_plugin")
        .summary("Read a plugin")
        .description(
            "Read a plugin descriptor. `id` is a built-in plugin GTS identifier or the UUID \
             of a custom plugin of the calling tenant.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin GTS identifier or UUID")
        .handler(handlers::get_plugin)
        .json_response_with_schema::<PluginResponseDto>(
            openapi,
            StatusCode::OK,
            "Plugin descriptor",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::delete("/oagw/v1/plugins/{id}")
        .operation_id("oagw.delete_plugin")
        .summary("Delete a custom plugin")
        .description(
            "Delete a custom plugin of the calling tenant. Rejects the deletion while an \
             upstream or route still references the plugin.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin UUID")
        .handler(handlers::delete_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "Plugin deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    OperationBuilder::get("/oagw/v1/plugins/{id}/source")
        .operation_id("oagw.get_plugin_source")
        .summary("Read the plugin source")
        .description(
            "Read the Starlark source of a custom plugin, or the documented contract of a \
             built-in plugin.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin GTS identifier or UUID")
        .handler(handlers::get_plugin_source)
        .json_response_with_schema::<PluginSourceResponseDto>(
            openapi,
            StatusCode::OK,
            "Plugin source",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi)
}

/// Declares the OData list parameters of a list operation
/// (`docs/DESIGN.md` §3.3 "List Query Parameters").
fn list_operation(operation: ListOperation) -> ListOperation {
    operation
        .tag(TAG)
        .query_param(
            "$filter",
            false,
            "OData filter, e.g. `alias eq 'api.openai.com'`",
        )
        .query_param("$select", false, "Comma-separated fields to return")
        .query_param("$orderby", false, "Sort order, e.g. `alias desc`")
        .query_param_typed(
            "$top",
            false,
            "Maximum number of items (default 50, max 100)",
            "integer",
        )
        .query_param_typed("$skip", false, "Number of items to skip", "integer")
}

/// Builder state of a list operation before auth is declared.
type ListOperation = OperationBuilder<Missing, Missing, (), AuthNotSet, LicenseNotSet>;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "routes_tests.rs"]
mod routes_tests;
