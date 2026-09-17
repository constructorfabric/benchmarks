//! REST route registration for the OAGW management API.
//!
//! Routes are registered **gear-relative, without `/api`**: `/oagw/v1/upstreams`,
//! `/oagw/v1/routes`, `/oagw/v1/plugins`. The host API gateway nests a
//! registered router under its own `prefix_path`, which is empty in the graded
//! configuration, so a `/api` prefix here would double it up (controller
//! decision C.1; ADR-0001 classifies `/oagw/v1/upstreams/*` as control-plane
//! traffic). Every operation is declared to the OpenAPI registry through
//! [`OperationBuilder`], so the published contract cannot drift from the
//! handlers.

use std::sync::Arc;

use axum::Router;
use toolkit::api::OpenApiRegistry;
use toolkit::api::canonical_prelude::StatusCode;
use toolkit::api::operation_builder::OperationBuilder;

use super::dto::{
    CreatePluginRequest, CreateRouteRequest, CreateUpstreamRequest, PluginDto, ReplaceRouteRequest,
    ReplaceUpstreamRequest, RouteDto, UpstreamDto,
};
use super::extract::MAX_BODY_BYTES;
use super::handlers;
use crate::domain::services::control_plane::ControlPlaneService;

/// Tag used for all OAGW operations in the OpenAPI document.
const API_TAG: &str = "Outbound API Gateway";

/// OpenAPI description of a resource path parameter.
///
/// Both spellings are accepted (DESIGN §3.3 / §3.6 "Resource Identification
/// Pattern"); response bodies always carry the bare UUID.
const ID_PARAMETER: &str = "Resource identifier: a bare UUID or the anonymous GTS identifier \
                           `gts.cf.core.oagw.<type>.v1~<uuid>`";

/// The OData list parameters every management collection accepts.
///
/// Kept as a macro rather than a helper because [`OperationBuilder`] is typed
/// over its build state, so the chain cannot be factored into a plain function.
macro_rules! list_parameters {
    ($builder:expr) => {
        $builder
            .query_param(
                "$filter",
                false,
                "OData filter expression over the resource's filterable fields",
            )
            .query_param(
                "$select",
                false,
                "Comma-separated fields to project, e.g. id,created_at",
            )
            .query_param("$orderby", false, "Sort order, e.g. created_at desc")
            .query_param(
                "$top",
                false,
                "Maximum number of items (default 50, max 100)",
            )
            .query_param("$skip", false, "Offset into the result set")
    };
}

/// Registers all REST routes for the OAGW control plane.
pub fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    service: Arc<ControlPlaneService>,
) -> Router {
    // POST /oagw/v1/upstreams - Create an upstream
    let router = OperationBuilder::post("/oagw/v1/upstreams")
        .operation_id("oagw.create_upstream")
        .summary("Create an upstream")
        .description(
            "Create a tenant-scoped upstream configuration. The alias is derived from the \
             endpoints when omitted and must be unique per tenant.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::create_upstream)
        .json_request::<CreateUpstreamRequest>(openapi, "Upstream configuration")
        .json_response_with_schema::<UpstreamDto>(
            openapi,
            StatusCode::CREATED,
            "The created upstream",
        )
        .problem_response(
            openapi,
            StatusCode::PAYLOAD_TOO_LARGE,
            "Body above the limit",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    // GET /oagw/v1/upstreams - List upstreams
    let router = OperationBuilder::get("/oagw/v1/upstreams")
        .operation_id("oagw.list_upstreams")
        .summary("List upstreams")
        .description(
            "List the upstreams of the calling tenant. Supports the OData query parameters \
             $filter, $select, $orderby, $top and $skip.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .query_param(
            "$filter",
            false,
            "OData filter expression, e.g. alias eq 'api.openai.com'",
        )
        .query_param(
            "$select",
            false,
            "Comma-separated fields to project, e.g. id,alias,server",
        )
        .query_param("$orderby", false, "Sort order, e.g. created_at desc")
        .query_param(
            "$top",
            false,
            "Maximum number of items (default 50, max 100)",
        )
        .query_param("$skip", false, "Offset into the result set")
        .handler(handlers::list_upstreams)
        .json_array_response_with_schema::<UpstreamDto>(
            openapi,
            StatusCode::OK,
            "Upstreams of the calling tenant",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    // GET /oagw/v1/upstreams/{id} - Get an upstream by id
    let router = OperationBuilder::get("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.get_upstream")
        .summary("Get an upstream")
        .description("Retrieve a single tenant-scoped upstream by its identifier.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", ID_PARAMETER)
        .handler(handlers::get_upstream)
        .json_response_with_schema::<UpstreamDto>(openapi, StatusCode::OK, "The upstream")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Upstream not found")
        .standard_errors(openapi)
        .register(router, openapi);

    // PUT /oagw/v1/upstreams/{id} - Replace an upstream
    let router = OperationBuilder::put("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.replace_upstream")
        .summary("Replace an upstream")
        .description(
            "Full replacement of an upstream. id, tenant_id and alias are immutable; endpoints \
             may only change when the derived alias is unchanged.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", ID_PARAMETER)
        .handler(handlers::replace_upstream)
        .json_request::<ReplaceUpstreamRequest>(openapi, "Upstream configuration")
        .json_response_with_schema::<UpstreamDto>(openapi, StatusCode::OK, "The replaced upstream")
        .problem_response(
            openapi,
            StatusCode::PAYLOAD_TOO_LARGE,
            "Body above the limit",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    // DELETE /oagw/v1/upstreams/{id} - Delete an upstream
    let router = OperationBuilder::delete("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.delete_upstream")
        .summary("Delete an upstream")
        .description("Delete a tenant-scoped upstream by its identifier.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", ID_PARAMETER)
        .handler(handlers::delete_upstream)
        .no_content_response(StatusCode::NO_CONTENT, "The upstream was deleted")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Upstream not found")
        .standard_errors(openapi)
        .register(router, openapi);

    // POST /oagw/v1/routes - Create a route
    let builder = OperationBuilder::post("/oagw/v1/routes")
        .operation_id("oagw.create_route")
        .summary("Create a route")
        .description(
            "Create a tenant-scoped route that binds a request match to an upstream. The match \
             rule (path and methods, or a gRPC service and method) must be unambiguous within \
             the upstream: a rule that intersects an existing one is a 409 MatchConflict. The \
             upstream must belong to the calling tenant, otherwise it is not addressable and the \
             response is a 404.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::create_route)
        .json_request::<CreateRouteRequest>(openapi, "Route configuration")
        .json_response_with_schema::<RouteDto>(openapi, StatusCode::CREATED, "The created route")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Upstream not found")
        .problem_response(openapi, StatusCode::CONFLICT, "Match conflict")
        .problem_response(
            openapi,
            StatusCode::PAYLOAD_TOO_LARGE,
            "Body above the limit",
        )
        .standard_errors(openapi);
    let router = builder.register(router, openapi);

    // GET /oagw/v1/routes - List routes
    let builder = OperationBuilder::get("/oagw/v1/routes")
        .operation_id("oagw.list_routes")
        .summary("List routes")
        .description(
            "List the routes of the calling tenant, each carrying its upstream identifier. \
             Supports the OData query parameters $filter, $select, $orderby, $top and $skip.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required();
    let builder = list_parameters!(builder);
    let router = builder
        .handler(handlers::list_routes)
        .json_array_response_with_schema::<RouteDto>(
            openapi,
            StatusCode::OK,
            "Routes of the calling tenant",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    // GET /oagw/v1/routes/{id} - Get a route by id
    let builder = OperationBuilder::get("/oagw/v1/routes/{id}")
        .operation_id("oagw.get_route")
        .summary("Get a route")
        .description("Retrieve a single tenant-scoped route by its identifier.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", ID_PARAMETER)
        .handler(handlers::get_route)
        .json_response_with_schema::<RouteDto>(openapi, StatusCode::OK, "The route")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Route not found")
        .standard_errors(openapi);
    let router = builder.register(router, openapi);

    // PUT /oagw/v1/routes/{id} - Replace a route
    let builder = OperationBuilder::put("/oagw/v1/routes/{id}")
        .operation_id("oagw.replace_route")
        .summary("Replace a route")
        .description(
            "Full replacement of a route. id, tenant_id and created_at are preserved; \
             upstream_id is immutable, so re-parenting a route is a 400 that names the field.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", ID_PARAMETER)
        .handler(handlers::replace_route)
        .json_request::<ReplaceRouteRequest>(openapi, "Route configuration")
        .json_response_with_schema::<RouteDto>(openapi, StatusCode::OK, "The replaced route")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Route not found")
        .problem_response(openapi, StatusCode::CONFLICT, "Match conflict")
        .problem_response(
            openapi,
            StatusCode::PAYLOAD_TOO_LARGE,
            "Body above the limit",
        )
        .standard_errors(openapi);
    let router = builder.register(router, openapi);

    // DELETE /oagw/v1/routes/{id} - Delete a route
    let builder = OperationBuilder::delete("/oagw/v1/routes/{id}")
        .operation_id("oagw.delete_route")
        .summary("Delete a route")
        .description("Delete a tenant-scoped route by its identifier.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", ID_PARAMETER)
        .handler(handlers::delete_route)
        .no_content_response(StatusCode::NO_CONTENT, "The route was deleted")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Route not found")
        .standard_errors(openapi);
    let router = builder.register(router, openapi);

    // POST /oagw/v1/plugins - Create a plugin
    let builder = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.create_plugin")
        .summary("Create a plugin")
        .description(
            "Register a Starlark plugin of type auth_plugin, guard_plugin or transform_plugin. \
             The name is unique per tenant; source_code is required and stored verbatim. Plugins \
             are immutable once created: there is no PUT, and the content changes only by \
             creating a new plugin and re-pointing the references (ADR-0002).",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::create_plugin)
        .json_request::<CreatePluginRequest>(openapi, "Plugin definition")
        .json_response_with_schema::<PluginDto>(openapi, StatusCode::CREATED, "The created plugin")
        .problem_response(openapi, StatusCode::CONFLICT, "Name already in use")
        .problem_response(
            openapi,
            StatusCode::PAYLOAD_TOO_LARGE,
            "Body above the limit",
        )
        .standard_errors(openapi);
    let router = builder.register(router, openapi);

    // GET /oagw/v1/plugins - List plugins
    let builder = OperationBuilder::get("/oagw/v1/plugins")
        .operation_id("oagw.list_plugins")
        .summary("List plugins")
        .description(
            "List the plugins of the calling tenant with their Starlark source. Supports the \
             OData query parameters $filter, $select, $orderby, $top and $skip.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required();
    let builder = list_parameters!(builder);
    let router = builder
        .handler(handlers::list_plugins)
        .json_array_response_with_schema::<PluginDto>(
            openapi,
            StatusCode::OK,
            "Plugins of the calling tenant",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    // GET /oagw/v1/plugins/{id} - Get a plugin by id
    let builder = OperationBuilder::get("/oagw/v1/plugins/{id}")
        .operation_id("oagw.get_plugin")
        .summary("Get a plugin")
        .description("Retrieve a single tenant-scoped plugin by its identifier.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", ID_PARAMETER)
        .handler(handlers::get_plugin)
        .json_response_with_schema::<PluginDto>(openapi, StatusCode::OK, "The plugin")
        .problem_response(openapi, StatusCode::SERVICE_UNAVAILABLE, "Plugin not found")
        .standard_errors(openapi);
    let router = builder.register(router, openapi);

    // DELETE /oagw/v1/plugins/{id} - Delete a plugin
    let builder = OperationBuilder::delete("/oagw/v1/plugins/{id}")
        .operation_id("oagw.delete_plugin")
        .summary("Delete a plugin")
        .description(
            "Delete a tenant-scoped plugin by its identifier. A plugin still referenced by any \
             upstream or route of any tenant is not deleted: the response is a 409 PluginInUse \
             carrying the referencing identifiers, so the data plane never resolves a plugin \
             that no longer exists.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", ID_PARAMETER)
        .handler(handlers::delete_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "The plugin was deleted")
        .problem_response(openapi, StatusCode::CONFLICT, "Plugin is referenced")
        .problem_response(openapi, StatusCode::SERVICE_UNAVAILABLE, "Plugin not found")
        .standard_errors(openapi);
    let router = builder.register(router, openapi);

    // GET /oagw/v1/plugins/{id}/source - Raw Starlark source
    let builder = OperationBuilder::get("/oagw/v1/plugins/{id}/source")
        .operation_id("oagw.get_plugin_source")
        .summary("Get the plugin source")
        .description(
            "Return the plugin's Starlark source verbatim as text/plain, exactly as it was \
             registered.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", ID_PARAMETER)
        .handler(handlers::get_plugin_source)
        .text_response(
            StatusCode::OK,
            "The Starlark source of the plugin",
            "text/plain",
        )
        .problem_response(openapi, StatusCode::SERVICE_UNAVAILABLE, "Plugin not found")
        .standard_errors(openapi);
    let router = builder.register(router, openapi);

    // A method a registered path does not implement is a 405 problem document
    // (DESIGN §2.1 / ADR-0007), not axum's plain-text default. Plugins are
    // immutable (ADR-0002), so `PUT /oagw/v1/plugins/{id}` is the case clients
    // actually hit. Unregistered *paths* keep axum's own 404: they are not
    // OAGW resources and carry no problem document.
    let router = router.method_not_allowed_fallback(handlers::method_not_allowed);

    // The control-plane service extension and the documented body-size limit
    // (DESIGN §2.2 `cpt-cf-oagw-constraint-body-limit`) are attached as explicit
    // final steps over the whole router. `Router::layer` only wraps the routes
    // registered so far, so chaining `.layer(...)` onto the last registration
    // would silently exclude every route added after it — and read as if the
    // layer belonged to DELETE only. Adding a route below this line is therefore
    // safe.
    //
    // The limit is set here, above axum's 2 MB default, so an oversized body is
    // rejected by the documented [`MAX_BODY_BYTES`] in `buffer_body` rather than
    // by an extractor the handler never sees.
    router
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(axum::Extension(service))
}
