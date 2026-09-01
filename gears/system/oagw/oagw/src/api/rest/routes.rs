//! REST route registration for the OAGW gear.
//!
//! * Management API (DESIGN §3.3 "Management API" — the 15 control-plane
//!   operations) is described in the `OpenAPI` document.
//! * Data plane (DESIGN §3.5 — `/proxy/{alias}` and `/metrics`) is registered
//!   as raw axum routes: a catch-all `{*path_suffix}` route carrying an
//!   arbitrary method cannot be modelled faithfully in the management
//!   document, and exposing it there would make a browser-discoverable
//!   `OPTIONS /proxy/{alias}` appear to be a management operation.

use std::sync::Arc;

use axum::{Extension, Router, routing::any, routing::get};
use toolkit::api::OpenApiRegistry;
use toolkit::api::canonical_prelude::StatusCode;
use toolkit::api::operation_builder::OperationBuilder;

use super::dto::{
    CreatePluginRequest, CreateRouteRequest, CreateUpstreamRequest, PluginDto, PluginSourceDto,
    ReplaceRouteRequest, ReplaceUpstreamRequest, RouteDto, UpstreamDto,
};
use super::handlers;
use crate::domain::service::ControlPlaneService;

const API_TAG: &str = "OAGW Management";
const BASE: &str = "/api/oagw/v1";

/// Registers the 15 management routes.
#[allow(clippy::needless_pass_by_value)]
pub fn register_routes(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
    service: Arc<ControlPlaneService>,
) -> Router {
    router = upstream_routes(router, openapi);
    router = route_routes(router, openapi);
    router = plugin_routes(router, openapi);
    router.layer(Extension(service))
}

/// Registers the data-plane routes (DESIGN §3.5).
///
/// Both the alias root and the suffixed shape are registered because axum's
/// `{*path_suffix}` capture does not match the empty suffix of
/// `/proxy/{alias}/`. The engine extension is layered on this sub-router only,
/// so the management operations never see it.
pub fn register_data_plane(router: Router, engine: handlers::proxy::Engine) -> Router {
    let proxied = any(handlers::proxy::proxy);
    let data_plane = Router::new()
        .route(&format!("{BASE}/proxy/{{alias}}"), proxied.clone())
        .route(&format!("{BASE}/proxy/{{alias}}/{{*path_suffix}}"), proxied)
        .route(&format!("{BASE}/metrics"), get(handlers::proxy::metrics))
        .layer(Extension(engine));
    router.merge(data_plane)
}

/// Registers the five `/upstreams` operations.
fn upstream_routes(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    // POST /upstreams
    router = OperationBuilder::post(format!("{BASE}/upstreams"))
        .operation_id("oagw.create_upstream")
        .summary("Create upstream")
        .description(
            "Create an upstream. The alias is derived from the endpoint pool unless an explicit \
             alias is supplied (mandatory for IP-based pools).",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<CreateUpstreamRequest>(openapi, "Upstream to create")
        .handler(handlers::upstreams::create_upstream)
        .json_response_with_schema::<UpstreamDto>(openapi, StatusCode::CREATED, "Created upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    // GET /upstreams
    router = OperationBuilder::get(format!("{BASE}/upstreams"))
        .operation_id("oagw.list_upstreams")
        .summary("List upstreams")
        .description("List the upstreams of the calling tenant.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .query_param("$filter", false, "OData filter, e.g. alias eq 'api.openai.com'")
        .query_param("$select", false, "Fields to return, e.g. id,alias,server")
        .query_param("$orderby", false, "Sort order, e.g. created_at desc")
        .query_param("$top", false, "Max results (default 50, max 100)")
        .query_param("$skip", false, "Offset for pagination")
        .handler(handlers::upstreams::list_upstreams)
        .json_response_with_schema::<toolkit_odata::Page<UpstreamDto>>(
            openapi,
            StatusCode::OK,
            "Paged upstreams",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    // GET /upstreams/{id}
    router = OperationBuilder::get(format!("{BASE}/upstreams/{{id}}"))
        .operation_id("oagw.get_upstream")
        .summary("Get upstream by ID")
        .description("Load a single upstream by its anonymous GTS identifier or bare UUID.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream identifier")
        .handler(handlers::upstreams::get_upstream)
        .json_response_with_schema::<UpstreamDto>(openapi, StatusCode::OK, "The upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    // PUT /upstreams/{id}
    router = OperationBuilder::put(format!("{BASE}/upstreams/{{id}}"))
        .operation_id("oagw.replace_upstream")
        .summary("Replace upstream")
        .description(
            "Full replacement. Omitted optional fields are cleared; the alias is recomputed when \
             hostname endpoints change.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream identifier")
        .json_request::<ReplaceUpstreamRequest>(openapi, "Replacement upstream")
        .handler(handlers::upstreams::replace_upstream)
        .json_response_with_schema::<UpstreamDto>(openapi, StatusCode::OK, "The upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    // DELETE /upstreams/{id}
    router = OperationBuilder::delete(format!("{BASE}/upstreams/{{id}}"))
        .operation_id("oagw.delete_upstream")
        .summary("Delete upstream")
        .description("Delete an upstream together with every route that references it.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream identifier")
        .handler(handlers::upstreams::delete_upstream)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .standard_errors(openapi)
        .register(router, openapi);

    router
}

/// Registers the five `/routes` operations.
fn route_routes(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    // POST /routes
    router = OperationBuilder::post(format!("{BASE}/routes"))
        .operation_id("oagw.create_route")
        .summary("Create route")
        .description("Create a route under an upstream owned by the calling tenant.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<CreateRouteRequest>(openapi, "Route to create")
        .handler(handlers::routes::create_route)
        .json_response_with_schema::<RouteDto>(openapi, StatusCode::CREATED, "Created route")
        .standard_errors(openapi)
        .register(router, openapi);

    // GET /routes
    router = OperationBuilder::get(format!("{BASE}/routes"))
        .operation_id("oagw.list_routes")
        .summary("List routes")
        .description("List the routes of the calling tenant.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .query_param("$filter", false, "OData filter, e.g. upstream_id eq '{uuid}'")
        .query_param("$select", false, "Fields to return")
        .query_param("$orderby", false, "Sort order, e.g. created_at desc")
        .query_param("$top", false, "Max results (default 50, max 100)")
        .query_param("$skip", false, "Offset for pagination")
        .handler(handlers::routes::list_routes)
        .json_response_with_schema::<toolkit_odata::Page<RouteDto>>(
            openapi,
            StatusCode::OK,
            "Paged routes",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    // GET /routes/{id}
    router = OperationBuilder::get(format!("{BASE}/routes/{{id}}"))
        .operation_id("oagw.get_route")
        .summary("Get route by ID")
        .description("Load a single route by its anonymous GTS identifier or bare UUID.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route identifier")
        .handler(handlers::routes::get_route)
        .json_response_with_schema::<RouteDto>(openapi, StatusCode::OK, "The route")
        .standard_errors(openapi)
        .register(router, openapi);

    // PUT /routes/{id}
    router = OperationBuilder::put(format!("{BASE}/routes/{{id}}"))
        .operation_id("oagw.replace_route")
        .summary("Replace route")
        .description(
            "Full replacement. `upstream_id` is immutable and therefore absent from the payload.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route identifier")
        .json_request::<ReplaceRouteRequest>(openapi, "Replacement route")
        .handler(handlers::routes::replace_route)
        .json_response_with_schema::<RouteDto>(openapi, StatusCode::OK, "The route")
        .standard_errors(openapi)
        .register(router, openapi);

    // DELETE /routes/{id}
    router = OperationBuilder::delete(format!("{BASE}/routes/{{id}}"))
        .operation_id("oagw.delete_route")
        .summary("Delete route")
        .description("Delete a route.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route identifier")
        .handler(handlers::routes::delete_route)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .standard_errors(openapi)
        .register(router, openapi);

    router
}

/// Registers the five `/plugins` operations.
///
/// Plugins are immutable (no `PUT`); `GET /plugins/{id}/source` returns the
/// Starlark source.
#[allow(clippy::needless_pass_by_value)]
fn plugin_routes(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    // POST /plugins
    router = OperationBuilder::post(format!("{BASE}/plugins"))
        .operation_id("oagw.create_plugin")
        .summary("Create plugin")
        .description("Create an immutable custom plugin (Starlark).")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<CreatePluginRequest>(openapi, "Plugin to create")
        .handler(handlers::plugins::create_plugin)
        .json_response_with_schema::<PluginDto>(openapi, StatusCode::CREATED, "Created plugin")
        .standard_errors(openapi)
        .register(router, openapi);

    // GET /plugins
    router = OperationBuilder::get(format!("{BASE}/plugins"))
        .operation_id("oagw.list_plugins")
        .summary("List plugins")
        .description("List the custom plugins of the calling tenant.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .query_param("$filter", false, "OData filter, e.g. type eq 'guard'")
        .query_param("$select", false, "Fields to return")
        .query_param("$orderby", false, "Sort order")
        .query_param("$top", false, "Max results")
        .query_param("$skip", false, "Offset for pagination")
        .handler(handlers::plugins::list_plugins)
        .json_response_with_schema::<toolkit_odata::Page<PluginDto>>(
            openapi,
            StatusCode::OK,
            "Paged plugins",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    // GET /plugins/{id}
    router = OperationBuilder::get(format!("{BASE}/plugins/{{id}}"))
        .operation_id("oagw.get_plugin")
        .summary("Get plugin by ID")
        .description("Load a single custom plugin.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin identifier")
        .handler(handlers::plugins::get_plugin)
        .json_response_with_schema::<PluginDto>(openapi, StatusCode::OK, "The plugin")
        .standard_errors(openapi)
        .register(router, openapi);

    // GET /plugins/{id}/source
    router = OperationBuilder::get(format!("{BASE}/plugins/{{id}}/source"))
        .operation_id("oagw.get_plugin_source")
        .summary("Get plugin source")
        .description("Return the sandboxed Starlark source of a custom plugin.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin identifier")
        .handler(handlers::plugins::get_plugin_source)
        .json_response_with_schema::<PluginSourceDto>(
            openapi,
            StatusCode::OK,
            "The plugin source",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    // DELETE /plugins/{id}
    router = OperationBuilder::delete(format!("{BASE}/plugins/{{id}}"))
        .operation_id("oagw.delete_plugin")
        .summary("Delete plugin")
        .description(
            "Delete a custom plugin. Returns 409 with `plugin_id` and `referenced_by` when the \
             plugin is still bound.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin identifier")
        .handler(handlers::plugins::delete_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .standard_errors(openapi)
        .register(router, openapi);

    router
}

