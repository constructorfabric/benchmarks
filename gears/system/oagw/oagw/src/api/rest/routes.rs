//! REST route registration for the OAGW gear.
//!
//! Management resources (upstreams / routes / plugins) are registered through
//! `OperationBuilder` so each operation is documented on the OpenAPI doc.
//! The data-plane proxy intake is a raw catch-all (`routing::any`) because it
//! spans all methods and forwards the unfiltered request to the data plane.

use std::sync::Arc;

use axum::routing;
use axum::{Extension, Router};
use toolkit::api::OpenApiRegistry;
use toolkit::api::canonical_prelude::StatusCode;
use toolkit::api::operation_builder::{
    CORE_GLOBAL_BASE_LICENSE_FEATURE, LicenseFeature, OperationBuilder,
};

use super::dto::{PluginDto, RouteDto, UpstreamDto};
use super::handlers;
use crate::domain::service::{ControlPlaneService, DataPlaneService};

/// Management API tag used in the OpenAPI document.
pub const API_TAG: &str = "OAGW Management";

/// License gate: core global base license feature (matches host policy).
struct License;

impl AsRef<str> for License {
    fn as_ref(&self) -> &'static str {
        CORE_GLOBAL_BASE_LICENSE_FEATURE
    }
}

impl LicenseFeature for License {}

/// Register all OAGW REST routes on `router`.
#[allow(clippy::needless_pass_by_value)]
pub fn register_routes(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
    control: Arc<ControlPlaneService>,
    data_plane: Arc<dyn DataPlaneService>,
) -> Router {
    // --- Upstreams ---
    router = OperationBuilder::post("/oagw/v1/upstreams")
        .operation_id("oagw.upstreams.create")
        .summary("Create upstream")
        .description("Register a new upstream service. The alias is auto-derived from hostname endpoints (ADR 0001); duplicate `(tenant, alias)` yields 409.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .json_request::<UpstreamDto>(openapi, "Upstream to create")
        .handler(handlers::create_upstream)
        .json_response_with_schema::<UpstreamDto>(openapi, StatusCode::CREATED, "Created upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams")
        .operation_id("oagw.upstreams.list")
        .summary("List upstreams")
        .description("List upstreams of the calling tenant. Supports `$top` and `$skip` pagination.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .handler(handlers::list_upstreams)
        .json_array_response_with_schema::<UpstreamDto>(openapi, StatusCode::OK, "List of upstreams")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.upstreams.get")
        .summary("Get upstream by ID")
        .description("Retrieve a single upstream. Ancestor resources are invisible (404).")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Upstream UUID identifier")
        .handler(handlers::get_upstream)
        .json_response_with_schema::<UpstreamDto>(openapi, StatusCode::OK, "The requested upstream")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Upstream not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.upstreams.update")
        .summary("Replace upstream")
        .description("Full replacement. `id`/`tenant_id` are ignored; the alias is immutable after creation.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Upstream UUID identifier")
        .json_request::<UpstreamDto>(openapi, "Replacement upstream")
        .handler(handlers::update_upstream)
        .json_response_with_schema::<UpstreamDto>(openapi, StatusCode::OK, "Updated upstream")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Upstream not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.upstreams.delete")
        .summary("Delete upstream")
        .description("Delete an upstream. Fails with 409 when routes still reference it.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Upstream UUID identifier")
        .handler(handlers::delete_upstream)
        .no_content_response(StatusCode::NO_CONTENT, "Upstream deleted")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Upstream not found")
        .problem_response(openapi, StatusCode::CONFLICT, "Upstream still referenced by routes")
        .standard_errors(openapi)
        .register(router, openapi);

    // --- Routes ---
    router = OperationBuilder::post("/oagw/v1/routes")
        .operation_id("oagw.routes.create")
        .summary("Create route")
        .description("Bind a match rule to an upstream of the same tenant. Conflicting match rules yield 409.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .json_request::<RouteDto>(openapi, "Route to create")
        .handler(handlers::create_route)
        .json_response_with_schema::<RouteDto>(openapi, StatusCode::CREATED, "Created route")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes")
        .operation_id("oagw.routes.list")
        .summary("List routes")
        .description("List routes of the calling tenant. Supports `$top` and `$skip` pagination.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .handler(handlers::list_routes)
        .json_array_response_with_schema::<RouteDto>(openapi, StatusCode::OK, "List of routes")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes/{id}")
        .operation_id("oagw.routes.get")
        .summary("Get route by ID")
        .description("Retrieve a single route. Ancestor resources are invisible (404).")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Route UUID identifier")
        .handler(handlers::get_route)
        .json_response_with_schema::<RouteDto>(openapi, StatusCode::OK, "The requested route")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Route not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/routes/{id}")
        .operation_id("oagw.routes.update")
        .summary("Replace route")
        .description("Full replacement. `id`/`tenant_id`/`upstream_id` are immutable.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Route UUID identifier")
        .json_request::<RouteDto>(openapi, "Replacement route")
        .handler(handlers::update_route)
        .json_response_with_schema::<RouteDto>(openapi, StatusCode::OK, "Updated route")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Route not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/routes/{id}")
        .operation_id("oagw.routes.delete")
        .summary("Delete route")
        .description("Delete a route binding.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Route UUID identifier")
        .handler(handlers::delete_route)
        .no_content_response(StatusCode::NO_CONTENT, "Route deleted")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Route not found")
        .standard_errors(openapi)
        .register(router, openapi);

    // --- Plugins ---
    router = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.plugins.create")
        .summary("Create plugin")
        .description("Create a custom (Starlark) plugin definition. Plugins are immutable after creation.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .json_request::<PluginDto>(openapi, "Plugin to create")
        .handler(handlers::create_plugin)
        .json_response_with_schema::<PluginDto>(openapi, StatusCode::CREATED, "Created plugin")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins")
        .operation_id("oagw.plugins.list")
        .summary("List plugins")
        .description("List custom plugin definitions of the calling tenant.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .handler(handlers::list_plugins)
        .json_array_response_with_schema::<PluginDto>(openapi, StatusCode::OK, "List of plugins")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{id}")
        .operation_id("oagw.plugins.get")
        .summary("Get plugin by ID")
        .description("Retrieve a single custom plugin definition.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Plugin UUID identifier")
        .handler(handlers::get_plugin)
        .json_response_with_schema::<PluginDto>(openapi, StatusCode::OK, "The requested plugin")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Plugin not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{id}/source")
        .operation_id("oagw.plugins.get_source")
        .summary("Get plugin Starlark source")
        .description("Retrieve the Starlark source of a custom plugin definition.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Plugin UUID identifier")
        .handler(handlers::get_plugin_source)
        .text_response(StatusCode::OK, "Plugin Starlark source", "text/plain")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Plugin not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/plugins/{id}")
        .operation_id("oagw.plugins.delete")
        .summary("Delete plugin")
        .description("Delete a custom plugin definition. Fails with 409 while referenced by an upstream or route.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Plugin UUID identifier")
        .handler(handlers::delete_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "Plugin deleted")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Plugin not found")
        .problem_response(openapi, StatusCode::CONFLICT, "Plugin still in use")
        .standard_errors(openapi)
        .register(router, openapi);

    // --- Data plane intake (raw catch-all) ---
    let proxy_handler = routing::any(handlers::proxy);
    router = router
        .route("/oagw/v1/proxy/{alias}", proxy_handler.clone())
        .route("/oagw/v1/proxy/{alias}/{*path}", proxy_handler);

    router.layer(Extension(control)).layer(Extension(data_plane))
}
