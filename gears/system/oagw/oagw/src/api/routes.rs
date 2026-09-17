//! REST route registration for the OAGW management API and the data-plane
//! proxy endpoint.
//!
//! Registration is gear-relative (`/oagw/v1/...`); api-gateway mounts the
//! resulting router under its own prefix when configured.
//!
//! * `/oagw/v1/upstreams` — upstream CRUD + list
//! * `/oagw/v1/routes` — route CRUD + list
//! * `/oagw/v1/plugins` — custom-plugin CRUD + list (immutable after create)
//! * `/oagw/v1/proxy/{alias}/{*rest}` — data plane (raw axum route)

use std::sync::Arc;

use axum::extract::Extension;
use axum::Router;
use http::StatusCode;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationBuilder;

use crate::api::dto::{PluginCreate, RouteCreate, RoutePut, UpstreamCreate};
use crate::api::handlers;
use crate::model::{CustomPlugin, Route, Upstream};
use crate::state::OagwState;

const API_TAG: &str = "OAGW";

/// Register every OAGW REST route and return the assembled router.
pub fn register_routes(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
    state: Arc<OagwState>,
) -> Router {
    // ------------------------------------------------------------------
    // Upstreams
    // ------------------------------------------------------------------

    router = OperationBuilder::post("/oagw/v1/upstreams")
        .operation_id("oagw.upstreams.create")
        .summary("Create an upstream")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<UpstreamCreate>(openapi, "Upstream to create")
        .handler(handlers::create_upstream)
        .json_response_with_schema::<Upstream>(openapi, StatusCode::CREATED, "Created upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams")
        .operation_id("oagw.upstreams.list")
        .summary("List upstreams")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .query_param("$filter", false, "OData-style filter expression")
        .query_param("$select", false, "Comma-separated field projection")
        .query_param("$orderby", false, "Sort field and direction")
        .query_param("$top", false, "Max results (default 50, max 100)")
        .query_param("$skip", false, "Offset for pagination")
        .handler(handlers::list_upstreams)
        .json_response(StatusCode::OK, "List of upstreams")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams/{upstream_id}")
        .operation_id("oagw.upstreams.get")
        .summary("Get an upstream by ID")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("upstream_id", "Upstream UUID")
        .handler(handlers::get_upstream)
        .json_response_with_schema::<Upstream>(openapi, StatusCode::OK, "The upstream")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Upstream not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/upstreams/{upstream_id}")
        .operation_id("oagw.upstreams.put")
        .summary("Replace an upstream")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("upstream_id", "Upstream UUID")
        .json_request::<UpstreamCreate>(openapi, "Replacement upstream")
        .handler(handlers::put_upstream)
        .json_response_with_schema::<Upstream>(openapi, StatusCode::OK, "Replaced upstream")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Upstream not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/upstreams/{upstream_id}")
        .operation_id("oagw.upstreams.delete")
        .summary("Delete an upstream")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("upstream_id", "Upstream UUID")
        .handler(handlers::delete_upstream)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Upstream not found")
        .standard_errors(openapi)
        .register(router, openapi);

    // ------------------------------------------------------------------
    // Routes
    // ------------------------------------------------------------------

    router = OperationBuilder::post("/oagw/v1/routes")
        .operation_id("oagw.routes.create")
        .summary("Create a route")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<RouteCreate>(openapi, "Route to create")
        .handler(handlers::create_route)
        .json_response_with_schema::<Route>(openapi, StatusCode::CREATED, "Created route")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes")
        .operation_id("oagw.routes.list")
        .summary("List routes")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .query_param("$filter", false, "OData-style filter expression")
        .query_param("$select", false, "Comma-separated field projection")
        .query_param("$orderby", false, "Sort field and direction")
        .query_param("$top", false, "Max results (default 50, max 100)")
        .query_param("$skip", false, "Offset for pagination")
        .handler(handlers::list_routes)
        .json_response(StatusCode::OK, "List of routes")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes/{route_id}")
        .operation_id("oagw.routes.get")
        .summary("Get a route by ID")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("route_id", "Route UUID")
        .handler(handlers::get_route)
        .json_response_with_schema::<Route>(openapi, StatusCode::OK, "The route")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Route not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/routes/{route_id}")
        .operation_id("oagw.routes.put")
        .summary("Replace a route")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("route_id", "Route UUID")
        .json_request::<RoutePut>(openapi, "Replacement route (upstream_id is immutable)")
        .handler(handlers::put_route)
        .json_response_with_schema::<Route>(openapi, StatusCode::OK, "Replaced route")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Route not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/routes/{route_id}")
        .operation_id("oagw.routes.delete")
        .summary("Delete a route")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("route_id", "Route UUID")
        .handler(handlers::delete_route)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Route not found")
        .standard_errors(openapi)
        .register(router, openapi);

    // ------------------------------------------------------------------
    // Custom plugins
    // ------------------------------------------------------------------

    router = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.plugins.create")
        .summary("Create a custom plugin")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<PluginCreate>(openapi, "Plugin to create")
        .handler(handlers::create_plugin)
        .json_response_with_schema::<CustomPlugin>(openapi, StatusCode::CREATED, "Created plugin")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins")
        .operation_id("oagw.plugins.list")
        .summary("List custom plugins")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .query_param("$filter", false, "OData-style filter expression")
        .query_param("$select", false, "Comma-separated field projection")
        .query_param("$orderby", false, "Sort field and direction")
        .query_param("$top", false, "Max results (default 50, max 100)")
        .query_param("$skip", false, "Offset for pagination")
        .handler(handlers::list_plugins)
        .json_response(StatusCode::OK, "List of plugins")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{plugin_id}")
        .operation_id("oagw.plugins.get")
        .summary("Get a custom plugin by ID")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("plugin_id", "Plugin UUID")
        .handler(handlers::get_plugin)
        .json_response_with_schema::<CustomPlugin>(openapi, StatusCode::OK, "The plugin")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Plugin not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/plugins/{plugin_id}")
        .operation_id("oagw.plugins.delete")
        .summary("Delete a custom plugin")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("plugin_id", "Plugin UUID")
        .handler(handlers::delete_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Plugin not found")
        .standard_errors(openapi)
        .register(router, openapi);

    // ------------------------------------------------------------------
    // Data plane
    // ------------------------------------------------------------------

    // Proxy: dynamic alias + wildcard suffix. `any` accepts every HTTP
    // method — including OPTIONS preflight, handled inside the data plane.
    router = router
        .route(
            "/oagw/v1/proxy/{alias}/{*rest}",
            axum::routing::any(handlers::proxy),
        )
        .route(
            "/oagw/v1/proxy/{alias}",
            axum::routing::any(handlers::proxy),
        );

    router.layer(Extension(state))
}
