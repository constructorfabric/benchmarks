//! Management API route registration.
//!
//! Every operation is declared through `OperationBuilder` so the OpenAPI
//! document and the axum router stay in lockstep. Paths are gear-relative
//! (`/oagw/v1/...`): the api-gateway nests gear routers under its own
//! `prefix_path`, which is empty in the graded configuration.

use std::sync::Arc;

use axum::Router;
use toolkit::api::OpenApiRegistry;
use toolkit::api::canonical_prelude::StatusCode;
use toolkit::api::operation_builder::{
    CORE_GLOBAL_BASE_LICENSE_FEATURE, LicenseFeature, OperationBuilder,
};

use crate::api::rest::dto::{
    CreatePluginRequest, CreateRouteRequest, CreateUpstreamRequest, PluginDto, ReplaceRouteRequest,
    ReplaceUpstreamRequest, RouteDto, UpstreamDto,
};
use crate::api::rest::handlers;
use crate::domain::services::OagwService;

/// Tag applied to every oagw operation in the OpenAPI document.
pub const API_TAG: &str = "oagw";

/// License feature of the management API.
///
/// The gear ships with the platform base feature: the marker is required by
/// `OperationBuilder`'s type state, the empty list keeps every deployment
/// allowed.
struct License;

impl AsRef<str> for License {
    fn as_ref(&self) -> &'static str {
        CORE_GLOBAL_BASE_LICENSE_FEATURE
    }
}

impl LicenseFeature for License {}

/// Registers the management API on the gear router.
#[allow(clippy::needless_pass_by_value)]
pub fn register_management_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    service: Arc<OagwService>,
) -> Router {
    let router = upstreams(router, openapi);
    let router = routes(router, openapi);
    let router = plugins(router, openapi);
    router.layer(axum::Extension(service))
}

fn upstreams(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    router = OperationBuilder::post("/oagw/v1/upstreams")
        .operation_id("oagw.create_upstream")
        .summary("Create an upstream")
        .description(
            "Create an outbound upstream. The alias is derived from a hostname \
             endpoint and must be given explicitly for IP endpoints.",
        )
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .json_request::<CreateUpstreamRequest>(openapi, "Upstream specification")
        .handler(handlers::upstreams::create_upstream)
        .json_response_with_schema::<UpstreamDto>(
            openapi,
            StatusCode::CREATED,
            "The created upstream",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams")
        .operation_id("oagw.list_upstreams")
        .summary("List upstreams")
        .description("List the upstreams of the calling tenant.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .query_param(
            "$top",
            false,
            "Maximum number of items (50 by default, 100 at most)",
        )
        .query_param("$skip", false, "Number of items to skip")
        .query_param(
            "$filter",
            false,
            "Single clause filter, e.g. alias eq 'api.openai.com'",
        )
        .query_param("$orderby", false, "Sort field, e.g. alias desc")
        .query_param("$select", false, "Comma separated projection fields")
        .handler(handlers::upstreams::list_upstreams)
        .json_response(StatusCode::OK, "Page of upstreams")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.get_upstream")
        .summary("Get an upstream")
        .description("Read a single upstream of the calling tenant.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "GTS identifier of the upstream")
        .handler(handlers::upstreams::get_upstream)
        .json_response_with_schema::<UpstreamDto>(openapi, StatusCode::OK, "The requested upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.replace_upstream")
        .summary("Replace an upstream")
        .description("Full replacement; omitted optional fields are cleared.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "GTS identifier of the upstream")
        .json_request::<ReplaceUpstreamRequest>(openapi, "Upstream specification")
        .handler(handlers::upstreams::replace_upstream)
        .json_response_with_schema::<UpstreamDto>(openapi, StatusCode::OK, "The replaced upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    OperationBuilder::delete("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.delete_upstream")
        .summary("Delete an upstream")
        .description("Delete an upstream together with its routes.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "GTS identifier of the upstream")
        .handler(handlers::upstreams::delete_upstream)
        .no_content_response(StatusCode::NO_CONTENT, "The upstream is gone")
        .standard_errors(openapi)
        .register(router, openapi)
}

fn routes(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    router = OperationBuilder::post("/oagw/v1/routes")
        .operation_id("oagw.create_route")
        .summary("Create a route")
        .description("Attach a match rule to an upstream of the calling tenant.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .json_request::<CreateRouteRequest>(openapi, "Route specification")
        .handler(handlers::routes::create_route)
        .json_response_with_schema::<RouteDto>(openapi, StatusCode::CREATED, "The created route")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes")
        .operation_id("oagw.list_routes")
        .summary("List routes")
        .description("List the routes of the calling tenant.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .query_param(
            "$top",
            false,
            "Maximum number of items (50 by default, 100 at most)",
        )
        .query_param("$skip", false, "Number of items to skip")
        .query_param(
            "$filter",
            false,
            "Single clause filter, e.g. tags eq 'chat'",
        )
        .query_param("$orderby", false, "Sort field, e.g. match.http.path desc")
        .query_param("$select", false, "Comma separated projection fields")
        .handler(handlers::routes::list_routes)
        .json_response(StatusCode::OK, "Page of routes")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes/{id}")
        .operation_id("oagw.get_route")
        .summary("Get a route")
        .description("Read a single route of the calling tenant.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "GTS identifier of the route")
        .handler(handlers::routes::get_route)
        .json_response_with_schema::<RouteDto>(openapi, StatusCode::OK, "The requested route")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/routes/{id}")
        .operation_id("oagw.replace_route")
        .summary("Replace a route")
        .description(
            "Full replacement of the route specification; the upstream \
             reference is immutable and absent from the body.",
        )
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "GTS identifier of the route")
        .json_request::<ReplaceRouteRequest>(openapi, "Route specification")
        .handler(handlers::routes::replace_route)
        .json_response_with_schema::<RouteDto>(openapi, StatusCode::OK, "The replaced route")
        .standard_errors(openapi)
        .register(router, openapi);

    OperationBuilder::delete("/oagw/v1/routes/{id}")
        .operation_id("oagw.delete_route")
        .summary("Delete a route")
        .description("Delete a match rule.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "GTS identifier of the route")
        .handler(handlers::routes::delete_route)
        .no_content_response(StatusCode::NO_CONTENT, "The route is gone")
        .standard_errors(openapi)
        .register(router, openapi)
}

fn plugins(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    router = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.create_plugin")
        .summary("Create a custom plugin")
        .description("Register a Starlark plugin of kind auth, guard or transform.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .json_request::<CreatePluginRequest>(openapi, "Plugin specification")
        .handler(handlers::plugins::create_plugin)
        .json_response_with_schema::<PluginDto>(openapi, StatusCode::CREATED, "The created plugin")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins")
        .operation_id("oagw.list_plugins")
        .summary("List custom plugins")
        .description("List the custom plugins of the calling tenant.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .query_param(
            "$top",
            false,
            "Maximum number of items (50 by default, 100 at most)",
        )
        .query_param("$skip", false, "Number of items to skip")
        .query_param(
            "$filter",
            false,
            "Single clause filter, e.g. plugin_type eq 'guard_plugin'",
        )
        .query_param("$orderby", false, "Sort field, e.g. name desc")
        .query_param("$select", false, "Comma separated projection fields")
        .handler(handlers::plugins::list_plugins)
        .json_response(StatusCode::OK, "Page of plugins")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{id}")
        .operation_id("oagw.get_plugin")
        .summary("Get a custom plugin")
        .description("Read a single custom plugin of the calling tenant.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "GTS identifier of the plugin")
        .handler(handlers::plugins::get_plugin)
        .json_response_with_schema::<PluginDto>(openapi, StatusCode::OK, "The requested plugin")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{id}/source")
        .operation_id("oagw.get_plugin_source")
        .summary("Get the plugin source")
        .description("Read the Starlark source of a custom plugin.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "GTS identifier of the plugin")
        .handler(handlers::plugins::get_plugin_source)
        .json_response(StatusCode::OK, "Plugin source")
        .standard_errors(openapi)
        .register(router, openapi);

    OperationBuilder::delete("/oagw/v1/plugins/{id}")
        .operation_id("oagw.delete_plugin")
        .summary("Delete a custom plugin")
        .description("Delete a custom plugin; 409 while a configuration binds it.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "GTS identifier of the plugin")
        .handler(handlers::plugins::delete_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "The plugin is gone")
        .standard_errors(openapi)
        .register(router, openapi)
}
