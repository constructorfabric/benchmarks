//! The OAGW management and proxy REST surface, mounted at `/oagw/v1`.
//!
//! The management resources are registered through the toolkit
//! `OperationBuilder` so they appear in the OpenAPI document; the proxy
//! catch-all is a plain route because its shape (`/{alias}/{*path_suffix}`) is
//! upstream-defined rather than enumerated here.

pub mod dto;
pub mod handlers;
pub mod query;

use std::sync::Arc;

use axum::Router;
use axum::routing::any;
use toolkit::api::OpenApiRegistry;
use toolkit::api::canonical_prelude::StatusCode;
use toolkit::api::operation_builder::OperationBuilder;

use crate::domain::model::{Plugin, Route, Upstream};
use crate::proxy::ProxyState;

const TAG: &str = "OAGW";

/// Registers the management and proxy routes.
pub fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    state: Arc<ProxyState>,
) -> Router {
    let router = register_upstreams(router, openapi);
    let router = register_routes_crud(router, openapi);
    let router = register_plugins(router, openapi);

    // ---- proxy (data plane) ---------------------------------------------
    // Registered after the management resources so the `Extension` layer (which
    // only applies to routes added before it) carries the state for all of them.
    router
        .route("/oagw/v1/proxy/{alias}", any(crate::proxy::handler_root))
        .route(
            "/oagw/v1/proxy/{alias}/{*path_suffix}",
            any(crate::proxy::handler),
        )
        .layer(axum::Extension(state))
}

fn register_upstreams(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    router = OperationBuilder::post("/oagw/v1/upstreams")
        .operation_id("oagw.create_upstream")
        .summary("Create an upstream")
        .description(
            "Creates an upstream in the calling tenant. The alias is the public \
             proxy handle; a derived alias is added when the declared alias does \
             not already encode the target host.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<Upstream>(openapi, "Upstream to create")
        .handler(handlers::create_upstream)
        .json_response_with_schema::<Upstream>(openapi, StatusCode::CREATED, "The created upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams")
        .operation_id("oagw.list_upstreams")
        .summary("List upstreams")
        .description(
            "Lists the upstreams owned by the calling tenant. Supports OData-style \
             `$filter`, `$select`, `$orderby`, `$top` and `$skip`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .query_param(
            "$filter",
            false,
            "Filter terms, AND-combined (`name eq 'x'`)",
        )
        .query_param("$select", false, "Comma-separated fields to project")
        .query_param("$orderby", false, "Sort keys, e.g. `alias desc`")
        .query_param("$top", false, "Page size (max 100, default 50)")
        .query_param("$skip", false, "Page offset")
        .handler(handlers::list_upstreams)
        .json_response_with_schema::<dto::ListResponse>(
            openapi,
            StatusCode::OK,
            "A page of upstreams",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.get_upstream")
        .summary("Read an upstream")
        .description("Reads one upstream owned by the calling tenant.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream identifier (alias or UUID)")
        .handler(handlers::get_upstream)
        .json_response_with_schema::<Upstream>(openapi, StatusCode::OK, "The upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.replace_upstream")
        .summary("Replace an upstream")
        .description(
            "Replaces an upstream owned by the calling tenant. The alias may not \
             change once derived, and descendants may not re-enable an \
             ancestor-disabled upstream.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream identifier (alias or UUID)")
        .json_request::<Upstream>(openapi, "Replacement upstream")
        .handler(handlers::replace_upstream)
        .json_response_with_schema::<Upstream>(openapi, StatusCode::OK, "The replaced upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.delete_upstream")
        .summary("Delete an upstream")
        .description("Deletes an upstream owned by the calling tenant.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream identifier (alias or UUID)")
        .handler(handlers::delete_upstream)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .standard_errors(openapi)
        .register(router, openapi);

    router
}

fn register_routes_crud(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    router = OperationBuilder::post("/oagw/v1/routes")
        .operation_id("oagw.create_route")
        .summary("Create a route")
        .description(
            "Creates a route binding a match rule to an upstream, with its own \
             plugin chain, CORS policy and rate limit.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<Route>(openapi, "Route to create")
        .handler(handlers::create_route)
        .json_response_with_schema::<Route>(openapi, StatusCode::CREATED, "The created route")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes")
        .operation_id("oagw.list_routes")
        .summary("List routes")
        .description(
            "Lists the routes owned by the calling tenant. Supports OData-style \
             `$filter`, `$select`, `$orderby`, `$top` and `$skip`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .query_param(
            "$filter",
            false,
            "Filter terms, AND-combined (`name eq 'x'`)",
        )
        .query_param("$select", false, "Comma-separated fields to project")
        .query_param("$orderby", false, "Sort keys, e.g. `match.base_path desc`")
        .query_param("$top", false, "Page size (max 100, default 50)")
        .query_param("$skip", false, "Page offset")
        .handler(handlers::list_routes)
        .json_response_with_schema::<dto::ListResponse>(openapi, StatusCode::OK, "A page of routes")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes/{id}")
        .operation_id("oagw.get_route")
        .summary("Read a route")
        .description("Reads one route owned by the calling tenant.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route identifier (UUID)")
        .handler(handlers::get_route)
        .json_response_with_schema::<Route>(openapi, StatusCode::OK, "The route")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/routes/{id}")
        .operation_id("oagw.replace_route")
        .summary("Replace a route")
        .description("Replaces a route owned by the calling tenant.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route identifier (UUID)")
        .json_request::<Route>(openapi, "Replacement route")
        .handler(handlers::replace_route)
        .json_response_with_schema::<Route>(openapi, StatusCode::OK, "The replaced route")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/routes/{id}")
        .operation_id("oagw.delete_route")
        .summary("Delete a route")
        .description("Deletes a route owned by the calling tenant.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route identifier (UUID)")
        .handler(handlers::delete_route)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .standard_errors(openapi)
        .register(router, openapi);

    router
}

fn register_plugins(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    router = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.create_plugin")
        .summary("Register a plugin")
        .description(
            "Registers a custom plugin definition. Starlark plugins are immutable \
             once created and are referenced by plugins bound to routes and upstreams.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<Plugin>(openapi, "Plugin to register")
        .handler(handlers::create_plugin)
        .json_response_with_schema::<Plugin>(openapi, StatusCode::CREATED, "The registered plugin")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins")
        .operation_id("oagw.list_plugins")
        .summary("List plugins")
        .description(
            "Lists the plugins owned by the calling tenant. Supports OData-style \
             `$filter`, `$select`, `$orderby`, `$top` and `$skip`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .query_param(
            "$filter",
            false,
            "Filter terms, AND-combined (`name eq 'x'`)",
        )
        .query_param("$select", false, "Comma-separated fields to project")
        .query_param("$orderby", false, "Sort keys, e.g. `name desc`")
        .query_param("$top", false, "Page size (max 100, default 50)")
        .query_param("$skip", false, "Page offset")
        .handler(handlers::list_plugins)
        .json_response_with_schema::<dto::ListResponse>(
            openapi,
            StatusCode::OK,
            "A page of plugins",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{id}")
        .operation_id("oagw.get_plugin")
        .summary("Read a plugin")
        .description("Reads one plugin owned by the calling tenant.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin identifier (name or UUID)")
        .handler(handlers::get_plugin)
        .json_response_with_schema::<Plugin>(openapi, StatusCode::OK, "The plugin")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{id}/source")
        .operation_id("oagw.get_plugin_source")
        .summary("Read a plugin's Starlark source")
        .description(
            "Returns the Starlark source of a custom plugin together with the phases \
             it hooks and the JSON schema of its configuration.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin identifier (name or UUID)")
        .handler(handlers::get_plugin_source)
        .json_response(StatusCode::OK, "The plugin source")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/plugins/{id}")
        .operation_id("oagw.delete_plugin")
        .summary("Delete a plugin")
        .description(
            "Deletes a plugin owned by the calling tenant. Fails with 409 while any \
             upstream or route still binds it.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin identifier (name or UUID)")
        .handler(handlers::delete_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .standard_errors(openapi)
        .register(router, openapi);

    router
}
