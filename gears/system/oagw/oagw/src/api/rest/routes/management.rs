// Updated: 2026-09-01 by Constructor Tech
//! The management API: upstreams, routes and plugins.

use axum::Router;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationBuilder;

use crate::api::rest::dto::{Page, PluginView, RouteView, UpstreamView};
use crate::api::rest::handlers;

const API_TAG: &str = "OAGW";

/// Register the management routes at the gear's API prefix.
pub fn register(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    // ── Upstreams ───────────────────────────────────────────────────────────
    router = OperationBuilder::get("/oagw/v1/upstreams")
        .operation_id("oagw.list_upstreams")
        .summary("List upstreams")
        .description("List the upstreams visible to the caller, across its tenant chain")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .query_param("$filter", false, "OData filter, e.g. tags eq 'ai'")
        .query_param("$orderby", false, "Sort key, optionally ` desc`")
        .query_param_typed("$top", false, "Page size", "integer")
        .query_param_typed("$skip", false, "Offset", "integer")
        .handler(handlers::upstreams::list)
        .json_response_with_schema::<Page<UpstreamView>>(
            openapi,
            http::StatusCode::OK,
            "A page of upstreams",
        )
        .error_400(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/oagw/v1/upstreams")
        .operation_id("oagw.create_upstream")
        .summary("Create an upstream")
        .description("Register an external service as an upstream, validating its endpoints")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<crate::domain::dto::Upstream>(openapi, "The upstream to create")
        .handler(handlers::upstreams::create)
        .json_response_with_schema::<UpstreamView>(
            openapi,
            http::StatusCode::CREATED,
            "The created upstream",
        )
        .error_400(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams/{upstream_id}")
        .operation_id("oagw.get_upstream")
        .summary("Read an upstream")
        .description("Read one upstream by identifier")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("upstream_id", "Upstream UUID")
        .handler(handlers::upstreams::get)
        .json_response_with_schema::<UpstreamView>(openapi, http::StatusCode::OK, "The upstream")
        .error_400(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/upstreams/{upstream_id}")
        .operation_id("oagw.replace_upstream")
        .summary("Replace an upstream")
        .description("Replace an upstream in full; the alias may not change")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("upstream_id", "Upstream UUID")
        .json_request::<crate::domain::dto::Upstream>(openapi, "The replacement upstream")
        .handler(handlers::upstreams::replace)
        .json_response_with_schema::<UpstreamView>(
            openapi,
            http::StatusCode::OK,
            "The replacement upstream",
        )
        .error_400(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/upstreams/{upstream_id}")
        .operation_id("oagw.delete_upstream")
        .summary("Delete an upstream")
        .description("Delete an upstream, cascading its routes")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("upstream_id", "Upstream UUID")
        .handler(handlers::upstreams::delete)
        .no_content_response(http::StatusCode::NO_CONTENT, "The upstream was deleted")
        .error_400(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // ── Routes ──────────────────────────────────────────────────────────────
    router = OperationBuilder::get("/oagw/v1/routes")
        .operation_id("oagw.list_routes")
        .summary("List routes")
        .description("List the routes visible to the caller")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .query_param("$filter", false, "OData filter, e.g. tags eq 'ai'")
        .query_param("$orderby", false, "Sort key, optionally ` desc`")
        .query_param_typed("$top", false, "Page size", "integer")
        .query_param_typed("$skip", false, "Offset", "integer")
        .handler(handlers::routes::list)
        .json_response_with_schema::<Page<RouteView>>(
            openapi,
            http::StatusCode::OK,
            "A page of routes",
        )
        .error_400(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/oagw/v1/routes")
        .operation_id("oagw.create_route")
        .summary("Create a route")
        .description("Attach a route, with its match rules and overrides, to an upstream")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<crate::domain::dto::Route>(openapi, "The route to create")
        .handler(handlers::routes::create)
        .json_response_with_schema::<RouteView>(
            openapi,
            http::StatusCode::CREATED,
            "The created route",
        )
        .error_400(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes/{route_id}")
        .operation_id("oagw.get_route")
        .summary("Read a route")
        .description("Read one route by identifier")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("route_id", "Route UUID")
        .handler(handlers::routes::get)
        .json_response_with_schema::<RouteView>(openapi, http::StatusCode::OK, "The route")
        .error_400(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/routes/{route_id}")
        .operation_id("oagw.replace_route")
        .summary("Replace a route")
        .description("Replace a route in full")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("route_id", "Route UUID")
        .json_request::<crate::domain::dto::Route>(openapi, "The replacement route")
        .handler(handlers::routes::replace)
        .json_response_with_schema::<RouteView>(
            openapi,
            http::StatusCode::OK,
            "The replacement route",
        )
        .error_400(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/routes/{route_id}")
        .operation_id("oagw.delete_route")
        .summary("Delete a route")
        .description("Delete a route")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("route_id", "Route UUID")
        .handler(handlers::routes::delete)
        .no_content_response(http::StatusCode::NO_CONTENT, "The route was deleted")
        .error_400(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // ── Plugins ─────────────────────────────────────────────────────────────
    router = OperationBuilder::get("/oagw/v1/plugins")
        .operation_id("oagw.list_plugins")
        .summary("List custom plugins")
        .description("List the custom plugins visible to the caller")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .query_param("$filter", false, "OData filter, e.g. type eq 'guard'")
        .query_param_typed("$top", false, "Page size", "integer")
        .query_param_typed("$skip", false, "Offset", "integer")
        .handler(handlers::plugins::list)
        .json_response_with_schema::<Page<PluginView>>(
            openapi,
            http::StatusCode::OK,
            "A page of plugins",
        )
        .error_400(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.create_plugin")
        .summary("Create a custom plugin")
        .description("Catalogue a custom plugin from its Starlark declaration")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<crate::api::rest::dto::CreatePluginRequest>(openapi, "The plugin to create")
        .handler(handlers::plugins::create)
        .json_response_with_schema::<PluginView>(
            openapi,
            http::StatusCode::CREATED,
            "The created plugin",
        )
        .error_400(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{plugin_id}")
        .operation_id("oagw.get_plugin")
        .summary("Read a custom plugin")
        .description("Read one custom plugin by identifier")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("plugin_id", "Plugin UUID")
        .handler(handlers::plugins::get)
        .json_response_with_schema::<PluginView>(openapi, http::StatusCode::OK, "The plugin")
        .error_400(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{plugin_id}/source")
        .operation_id("oagw.get_plugin_source")
        .summary("Read a plugin's declaration")
        .description("Return the Starlark declaration a custom plugin was created with")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("plugin_id", "Plugin UUID")
        .handler(handlers::plugins::source)
        .json_response_with_schema::<crate::api::rest::dto::PluginSourceView>(
            openapi,
            http::StatusCode::OK,
            "The plugin declaration",
        )
        .error_400(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/plugins/{plugin_id}")
        .operation_id("oagw.delete_plugin")
        .summary("Delete a custom plugin")
        .description("Delete an unreferenced custom plugin; a referenced one is a 409")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("plugin_id", "Plugin UUID")
        .handler(handlers::plugins::delete)
        .no_content_response(http::StatusCode::NO_CONTENT, "The plugin was deleted")
        .error_400(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router
}
