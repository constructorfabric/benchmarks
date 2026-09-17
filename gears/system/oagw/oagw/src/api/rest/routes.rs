//! REST route registration via [`OperationBuilder`](toolkit::api::operation_builder::OperationBuilder).
//!
//! All operations are gear-relative under `/oagw/v1` (see `api/mod.rs`).
//! Every operation is `.authenticated()` and `.no_license_required()` —
//! authorization is enforced by the service layer via
//! [`ControlPlane`](crate::domain::services::ControlPlane) scope checks
//! (OAGW is licensed via the core global base feature, so no separate
//! license gating applies). Request bodies are registered by schema name
//! (records double as wire DTOs and are not `utoipa::ToSchema`), and
//! responses are schema-less JSON.
//!
//! Note on paths: the proxy catch-all is registered ONCE with a
//! [`MethodRouter`] that handles GET/POST/PUT/PATCH/DELETE/HEAD/OPTIONS,
//! because `OperationBuilder::get` alone would only install a GET route.
//! OPTIONS preflights are answered inside the shared handler per ADR 0004;
//! the gateway auth middleware lets anonymous preflights through.

use std::sync::Arc;

use axum::routing;
use axum::Router;
use toolkit::api::operation_builder::{OperationBuilder, OperationBuilderODataExt};
use toolkit::api::OpenApiRegistry;

use crate::api::rest::{dto, handlers};
use crate::domain::services::ControlPlane;
use crate::infra::proxy::DataPlane;

const API_TAG: &str = "OAGW";

/// Register every OAGW route (control plane + data plane catch-all) and
/// attach the shared service handles as Extension layers.
#[allow(clippy::needless_pass_by_value)]
pub fn register_routes(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
    control_plane: Arc<ControlPlane>,
    data_plane: Arc<DataPlane>,
) -> Router {
    // =================================================================
    //                            Upstreams
    // =================================================================

    router = OperationBuilder::get("/oagw/v1/upstreams")
        .operation_id("oagw.list_upstreams")
        .summary("List upstreams")
        .description("List OAGW upstreams with OData filter/select/orderby and cursor paging")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .query_param_typed(
            "limit",
            false,
            "Maximum number of upstreams to return (default 50, max 100)",
            "integer",
        )
        .query_param("cursor", false, "Cursor for pagination")
        .handler(handlers::list_upstreams)
        .json_response(http::StatusCode::OK, "List of upstreams")
        .with_odata_filter::<dto::UpstreamField>()
        .with_odata_select()
        .with_odata_orderby::<dto::UpstreamField>()
        .error_400(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/oagw/v1/upstreams")
        .operation_id("oagw.create_upstream")
        .summary("Create an upstream")
        .description("Register a new OAGW upstream")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request_schema("OagwUpstreamRequest", "Upstream creation data")
        .handler(handlers::create_upstream)
        .json_response(http::StatusCode::CREATED, "Created upstream")
        .error_400(openapi)
        .error_403(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.get_upstream")
        .summary("Get an upstream")
        .description("Retrieve one upstream by id")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream id")
        .handler(handlers::get_upstream)
        .json_response(http::StatusCode::OK, "Upstream found")
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.replace_upstream")
        .summary("Replace an upstream")
        .description("Full replacement of an upstream (alias is immutable once set)")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream id")
        .json_request_schema("OagwUpstreamRequest", "Upstream replacement data")
        .handler(handlers::replace_upstream)
        .json_response(http::StatusCode::OK, "Replaced upstream")
        .error_400(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.delete_upstream")
        .summary("Delete an upstream")
        .description("Delete an upstream (and its routes)")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream id")
        .handler(handlers::delete_upstream)
        .no_content_response(http::StatusCode::NO_CONTENT, "Upstream deleted")
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // =================================================================
    //                             Routes
    // =================================================================

    router = OperationBuilder::get("/oagw/v1/routes")
        .operation_id("oagw.list_routes")
        .summary("List routes")
        .description("List OAGW routes with OData filter/select/orderby and cursor paging")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .query_param_typed(
            "limit",
            false,
            "Maximum number of routes to return (default 50, max 100)",
            "integer",
        )
        .query_param("cursor", false, "Cursor for pagination")
        .handler(handlers::list_routes)
        .json_response(http::StatusCode::OK, "List of routes")
        .with_odata_filter::<dto::RouteField>()
        .with_odata_select()
        .with_odata_orderby::<dto::RouteField>()
        .error_400(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/oagw/v1/routes")
        .operation_id("oagw.create_route")
        .summary("Create a route")
        .description("Register a new OAGW route bound to an upstream")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request_schema("OagwRouteRequest", "Route creation data")
        .handler(handlers::create_route)
        .json_response(http::StatusCode::CREATED, "Created route")
        .error_400(openapi)
        .error_403(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes/{id}")
        .operation_id("oagw.get_route")
        .summary("Get a route")
        .description("Retrieve one route by id")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route id")
        .handler(handlers::get_route)
        .json_response(http::StatusCode::OK, "Route found")
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/routes/{id}")
        .operation_id("oagw.replace_route")
        .summary("Replace a route")
        .description("Full replacement of a route (upstream_id is immutable)")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route id")
        .json_request_schema("OagwRouteUpdateRequest", "Route replacement data")
        .handler(handlers::replace_route)
        .json_response(http::StatusCode::OK, "Replaced route")
        .error_400(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/routes/{id}")
        .operation_id("oagw.delete_route")
        .summary("Delete a route")
        .description("Delete a route")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route id")
        .handler(handlers::delete_route)
        .no_content_response(http::StatusCode::NO_CONTENT, "Route deleted")
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // =================================================================
    //                             Plugins
    // =================================================================

    router = OperationBuilder::get("/oagw/v1/plugins")
        .operation_id("oagw.list_plugins")
        .summary("List plugins")
        .description("List OAGW plugins with OData filter/select/orderby and cursor paging")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .query_param_typed(
            "limit",
            false,
            "Maximum number of plugins to return (default 50, max 100)",
            "integer",
        )
        .query_param("cursor", false, "Cursor for pagination")
        .handler(handlers::list_plugins)
        .json_response(http::StatusCode::OK, "List of plugins")
        .with_odata_filter::<dto::PluginField>()
        .with_odata_select()
        .with_odata_orderby::<dto::PluginField>()
        .error_400(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.create_plugin")
        .summary("Create a plugin")
        .description("Register a new OAGW plugin (auth, guard, or transform)")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request_schema("OagwPluginCreateRequest", "Plugin creation data")
        .handler(handlers::create_plugin)
        .json_response(http::StatusCode::CREATED, "Created plugin")
        .error_400(openapi)
        .error_403(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{id}")
        .operation_id("oagw.get_plugin")
        .summary("Get a plugin")
        .description("Retrieve one plugin by id")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin id")
        .handler(handlers::get_plugin)
        .json_response(http::StatusCode::OK, "Plugin found")
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{id}/source")
        .operation_id("oagw.get_plugin_source")
        .summary("Get plugin source")
        .description("Retrieve the Starlark source stanza of a plugin")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin id")
        .handler(handlers::get_plugin_source)
        .json_response(http::StatusCode::OK, "Plugin source")
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/plugins/{id}")
        .operation_id("oagw.delete_plugin")
        .summary("Delete a plugin")
        .description("Delete a plugin (409 when still referenced by an upstream)")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin id")
        .handler(handlers::delete_plugin)
        .no_content_response(http::StatusCode::NO_CONTENT, "Plugin deleted")
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // =================================================================
    //                             Proxy
    // =================================================================

    // Single wildcard route carrying all seven methods. OPTIONS is used
    // for CORS preflights, answered permissively inside the handler per
    // ADR 0004; GET..DELETE + HEAD exercise the real data plane.
    router = OperationBuilder::get("/oagw/v1/proxy/{*path}")
        .operation_id("oagw.proxy")
        .summary("Proxy a request to an upstream")
        .description(
            "Resolve the upstream alias from the path and proxy the request through the \
             configured auth/guard/transform plugins, rate limits, CORS, and target host rules",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("path", "Upstream alias and optional path suffix")
        .method_router(
            routing::get(handlers::proxy)
                .post(handlers::proxy)
                .put(handlers::proxy)
                .patch(handlers::proxy)
                .delete(handlers::proxy)
                .head(handlers::proxy)
                .options(handlers::proxy),
        )
        .json_response(http::StatusCode::OK, "Proxied upstream response")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .problem_response(
            openapi,
            http::StatusCode::PAYLOAD_TOO_LARGE,
            "Request payload exceeds the proxy body limit",
        )
        .error_429(openapi)
        .error_500(openapi)
        .error_502(openapi)
        .error_503(openapi)
        .error_504(openapi)
        .register(router, openapi);

    router = router
        .layer(axum::Extension(control_plane))
        .layer(axum::Extension(data_plane));

    router
}
