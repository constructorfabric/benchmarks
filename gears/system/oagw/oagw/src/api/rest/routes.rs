//! REST route registration for the OAGW gear.
//!
//! Routes are gear-relative with no `/api` segment (ADR:
//! `cpt-cf-oagw-constraint-no-api-segment`): `/oagw/v1/upstreams`,
//! `/oagw/v1/routes`, `/oagw/v1/plugins` for management and
//! `{METHOD} /oagw/v1/proxy/{alias}/{*rest}` for proxying. Management routes
//! are tenant-scoped and authz-gated in the handlers; the proxy route is a
//! raw axum route because its method set is unbounded.

use std::sync::Arc;

use axum::Extension;
use axum::Router;
use axum::routing::any;
use toolkit::api::OpenApiRegistry;
use toolkit::api::canonical_prelude::StatusCode;
use toolkit::api::operation_builder::OperationBuilder;

use crate::api::rest::{ProxyPort, dto, handlers};
use crate::domain::repository::ControlPlaneService;
use authz_resolver_sdk::pep::PolicyEnforcer;

const API_TAG: &str = "OAGW Control Plane";

/// Registers the OAGW REST surface onto `router`.
#[allow(clippy::needless_pass_by_value)]
pub fn register_routes(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
    control: Arc<ControlPlaneService>,
    enforcer: Arc<PolicyEnforcer>,
    port: ProxyPort,
) -> Router {
    // --- upstreams ------------------------------------------------------
    router = OperationBuilder::post("/oagw/v1/upstreams")
        .operation_id("oagw.upstreams.create")
        .summary("Create an upstream")
        .description("Create or replace an upstream registration (idempotent by alias).")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<dto::UpstreamRequest>(openapi, "Upstream to create")
        .handler(handlers::create_upstream)
        .json_response_with_schema::<dto::UpstreamDto>(
            openapi,
            StatusCode::CREATED,
            "The created upstream",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams")
        .operation_id("oagw.upstreams.list")
        .summary("List upstreams")
        .description("List all upstreams visible to the caller's tenant.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::list_upstreams)
        .json_array_response_with_schema::<dto::UpstreamDto>(openapi, StatusCode::OK, "Upstreams")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams/{alias}")
        .operation_id("oagw.upstreams.get")
        .summary("Get an upstream")
        .description("Retrieve a single upstream by alias.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("alias", "The upstream alias")
        .handler(handlers::get_upstream)
        .json_response_with_schema::<dto::UpstreamDto>(openapi, StatusCode::OK, "The upstream")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Upstream not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/upstreams/{alias}")
        .operation_id("oagw.upstreams.update")
        .summary("Update an upstream")
        .description("Update an upstream by alias.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("alias", "The upstream alias")
        .json_request::<dto::UpstreamRequest>(openapi, "Updated upstream")
        .handler(handlers::update_upstream)
        .json_response_with_schema::<dto::UpstreamDto>(
            openapi,
            StatusCode::OK,
            "The updated upstream",
        )
        .problem_response(openapi, StatusCode::NOT_FOUND, "Upstream not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/upstreams/{alias}")
        .operation_id("oagw.upstreams.delete")
        .summary("Delete an upstream")
        .description("Delete an upstream by alias.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("alias", "The upstream alias")
        .handler(handlers::delete_upstream)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Upstream not found")
        .standard_errors(openapi)
        .register(router, openapi);

    // --- routes ---------------------------------------------------------
    router = OperationBuilder::post("/oagw/v1/routes")
        .operation_id("oagw.routes.create")
        .summary("Create a route")
        .description("Create or replace a route registration (idempotent by alias).")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<dto::RouteRequest>(openapi, "Route to create")
        .handler(handlers::create_route)
        .json_response_with_schema::<dto::RouteDto>(
            openapi,
            StatusCode::CREATED,
            "The created route",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes")
        .operation_id("oagw.routes.list")
        .summary("List routes")
        .description("List all routes visible to the caller's tenant.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::list_routes)
        .json_array_response_with_schema::<dto::RouteDto>(openapi, StatusCode::OK, "Routes")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes/{alias}")
        .operation_id("oagw.routes.get")
        .summary("Get a route")
        .description("Retrieve a single route by alias.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("alias", "The route alias")
        .handler(handlers::get_route)
        .json_response_with_schema::<dto::RouteDto>(openapi, StatusCode::OK, "The route")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Route not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/routes/{alias}")
        .operation_id("oagw.routes.update")
        .summary("Update a route")
        .description("Update a route by alias.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("alias", "The route alias")
        .json_request::<dto::RouteRequest>(openapi, "Updated route")
        .handler(handlers::update_route)
        .json_response_with_schema::<dto::RouteDto>(openapi, StatusCode::OK, "The updated route")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Route not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/routes/{alias}")
        .operation_id("oagw.routes.delete")
        .summary("Delete a route")
        .description("Delete a route by alias.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("alias", "The route alias")
        .handler(handlers::delete_route)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Route not found")
        .standard_errors(openapi)
        .register(router, openapi);

    // --- plugins --------------------------------------------------------
    router = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.plugins.create")
        .summary("Create a plugin instance")
        .description("Create or replace a plugin instance (idempotent by alias).")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<dto::PluginRequest>(openapi, "Plugin to create")
        .handler(handlers::create_plugin)
        .json_response_with_schema::<dto::PluginDto>(
            openapi,
            StatusCode::CREATED,
            "The created plugin",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins")
        .operation_id("oagw.plugins.list")
        .summary("List plugins")
        .description("List all plugin instances visible to the caller's tenant.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::list_plugins)
        .json_array_response_with_schema::<dto::PluginDto>(openapi, StatusCode::OK, "Plugins")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{alias}")
        .operation_id("oagw.plugins.get")
        .summary("Get a plugin instance")
        .description("Retrieve a single plugin instance by alias.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("alias", "The plugin alias")
        .handler(handlers::get_plugin)
        .json_response_with_schema::<dto::PluginDto>(openapi, StatusCode::OK, "The plugin")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Plugin not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/plugins/{alias}")
        .operation_id("oagw.plugins.update")
        .summary("Update a plugin instance")
        .description("Update a plugin instance by alias.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("alias", "The plugin alias")
        .json_request::<dto::PluginRequest>(openapi, "Updated plugin")
        .handler(handlers::update_plugin)
        .json_response_with_schema::<dto::PluginDto>(openapi, StatusCode::OK, "The updated plugin")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Plugin not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/plugins/{alias}")
        .operation_id("oagw.plugins.delete")
        .summary("Delete a plugin instance")
        .description("Delete a plugin instance by alias.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("alias", "The plugin alias")
        .handler(handlers::delete_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Plugin not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/oagw/v1/plugins/{alias}/bind")
        .operation_id("oagw.plugins.bind")
        .summary("Bind a plugin")
        .description("Bind a plugin instance to an upstream or a route.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("alias", "The plugin alias")
        .json_request::<dto::BindRequest>(openapi, "Binding target (upstream or route)")
        .handler(handlers::bind_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "Bound")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/oagw/v1/plugins/{alias}/unbind")
        .operation_id("oagw.plugins.unbind")
        .summary("Unbind a plugin")
        .description("Unbind a plugin instance from an upstream or a route.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("alias", "The plugin alias")
        .json_request::<dto::BindRequest>(openapi, "Binding target (upstream or route)")
        .handler(handlers::unbind_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "Unbound")
        .standard_errors(openapi)
        .register(router, openapi);

    // --- proxy surface (dynamic route, bypasses the operation builder) --
    router = router.route("/oagw/v1/proxy/{alias}/{*path}", any(handlers::proxy_relay));

    router
        .layer(Extension(control))
        .layer(Extension(enforcer))
        .layer(Extension(port))
}
