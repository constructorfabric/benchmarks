//! REST route registration for the OAGW gear.
//!
//! `register_routes` merges this gear's routes onto the router it is given
//! and returns it (`cpt-cf-oagw-dod-router-mount`); it never nests a fresh
//! sub-router. Every path registered here is gear-relative
//! (`/oagw/v1/...`) and must never repeat the `/api` prefix an
//! operator-facing gateway may apply in front of this gear — the api-gateway
//! gear nests the composed router under its own `prefix_path`, which is
//! empty in the graded configuration.
//!
//! `cpt-cf-oagw-feature-upstream-management` is the first feature to extend
//! this function with concrete endpoints, following the `OperationBuilder`
//! pattern (`.authenticated()`, `types-registry`'s
//! `src/api/rest/routes.rs`) — authentication/authorization enforcement
//! itself stays the platform gateway's responsibility ahead of this gear,
//! per the feature's boundary note; `.authenticated()` here only records
//! that fact for the `OpenAPI` document. The catch-all fallback that proves
//! the mount is live still renders the standard `RouteNotFound` problem
//! document for any `/oagw/v1/...` path not claimed by a more specific route
//! below (`cpt-cf-oagw-dod-unmounted-path-fallback`).

use axum::Router;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationBuilder;

use crate::api::rest::handlers::plugins as plugin_handlers;
use crate::api::rest::handlers::proxy as proxy_handlers;
use crate::api::rest::handlers::routes as route_handlers;
use crate::api::rest::handlers::upstreams;
use crate::domain::model::{
    Plugin, PluginRequest, PluginSource, Route, RouteRequest, Upstream, UpstreamRequest,
};
use crate::error::OagwError;

/// Gear-relative mount path (`cpt-cf-oagw-dod-router-mount`).
const MOUNT_PATH: &str = "/oagw/v1";

/// `OpenAPI` tag for the upstream management endpoints.
const UPSTREAMS_TAG: &str = "OAGW Upstreams";

/// `OpenAPI` tag for the route management endpoints.
const ROUTES_TAG: &str = "OAGW Routes";

/// `OpenAPI` tag for the plugin management endpoints.
const PLUGINS_TAG: &str = "OAGW Plugins";

/// Registers this gear's REST routes onto the router it is given, and
/// returns it.
pub fn register_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    // This gear's own routes are composed into a separate router first, so the
    // error-source layer below binds to them alone. `router` is the router the
    // host shares across every gear, and layering onto it directly would stamp
    // other gears' responses too.
    let own = register_upstream_routes(Router::new(), openapi);
    let own = register_route_routes(own, openapi);
    let own = register_plugin_routes(own, openapi);
    let own = register_proxy_routes(own);

    // @cpt-begin:cpt-cf-oagw-dod-router-mount:p1:inst-router-mount-attach-01
    let wildcard_path = format!("{MOUNT_PATH}/{{*rest}}");
    let own = own
        .route(MOUNT_PATH, any(route_not_found))
        .route(&wildcard_path, any(route_not_found));
    // @cpt-end:cpt-cf-oagw-dod-router-mount:p1:inst-router-mount-attach-01

    // @cpt-begin:cpt-cf-oagw-dod-error-source-header:p1:inst-error-source-blanket-middleware-01
    // Every response this gear returns must carry `X-OAGW-Error-Source`,
    // success or failure (`cpt-cf-oagw-dod-error-source-header`).
    // `OagwError::into_response` and `stamp_upstream_source` already cover
    // every failure and every proxied response respectively; this layer fills
    // in `gateway` for what is left — a successful management-endpoint
    // response — without overriding either of those. It is scoped to this
    // gear's own routes, never to the shared host router.
    let own = own.layer(middleware::from_fn(ensure_error_source_header));
    // @cpt-end:cpt-cf-oagw-dod-error-source-header:p1:inst-error-source-blanket-middleware-01

    router.merge(own)
}

/// Applies [`crate::error::ensure_gateway_source`] to every response this
/// gear's router produces.
async fn ensure_error_source_header(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    crate::error::ensure_gateway_source(&mut response);
    response
}

/// Registers the `/oagw/v1/upstreams` CRUD endpoints
/// (`cpt-cf-oagw-feature-upstream-management`).
// @cpt-begin:cpt-cf-oagw-dod-error-responses:p1:inst-upstream-routes-register-01
fn register_upstream_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::post("/oagw/v1/upstreams")
        .operation_id("oagw.upstreams.create")
        .summary("Create an upstream")
        .description("Validates, derives or validates the alias, checks uniqueness, and persists a new upstream.")
        .tag(UPSTREAMS_TAG)
        .authenticated()
        .no_license_required()
        .json_request_no_desc::<UpstreamRequest>(openapi)
        .handler(upstreams::create_upstream)
        .json_response_with_schema::<Upstream>(openapi, StatusCode::CREATED, "The created upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/upstreams")
        .operation_id("oagw.upstreams.list")
        .summary("List upstreams")
        .description("Lists the calling tenant's own upstreams, honoring $filter, $select, $orderby, $top, and $skip.")
        .tag(UPSTREAMS_TAG)
        .authenticated()
        .no_license_required()
        .query_param("$filter", false, "OData-style 'field eq value' filter clause")
        .query_param("$select", false, "Comma-separated list of fields to return")
        .query_param("$orderby", false, "'field' or 'field asc|desc'")
        .query_param_typed("$top", false, "Max results (default 50, max 100)", "integer")
        .query_param_typed("$skip", false, "Offset for pagination", "integer")
        .handler(upstreams::list_upstreams)
        .json_response(StatusCode::OK, "The filtered, paginated list of upstreams")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.upstreams.get")
        .summary("Get an upstream by id")
        .tag(UPSTREAMS_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "The upstream's UUID")
        .handler(upstreams::get_upstream)
        .json_response_with_schema::<Upstream>(openapi, StatusCode::OK, "The requested upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::put("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.upstreams.replace")
        .summary("Replace an upstream")
        .description("Full-field replacement; omitted optional fields are cleared. The alias and id are immutable.")
        .tag(UPSTREAMS_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "The upstream's UUID")
        .json_request_no_desc::<UpstreamRequest>(openapi)
        .handler(upstreams::replace_upstream)
        .json_response_with_schema::<Upstream>(openapi, StatusCode::OK, "The replaced upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    OperationBuilder::delete("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.upstreams.delete")
        .summary("Delete an upstream")
        .tag(UPSTREAMS_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "The upstream's UUID")
        .handler(upstreams::delete_upstream)
        .json_response(StatusCode::NO_CONTENT, "Deleted")
        .standard_errors(openapi)
        .register(router, openapi)
}
// @cpt-end:cpt-cf-oagw-dod-error-responses:p1:inst-upstream-routes-register-01

/// Registers the `/oagw/v1/routes` CRUD endpoints
/// (`cpt-cf-oagw-feature-route-management`,
/// `cpt-cf-oagw-dod-route-crud-endpoints`).
// @cpt-begin:cpt-cf-oagw-dod-route-crud-endpoints:p1:inst-route-routes-register-01
fn register_route_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::post("/oagw/v1/routes")
        .operation_id("oagw.routes.create")
        .summary("Create a route")
        .description(
            "Validates the payload against route.v1.schema.json, resolves upstream_id against \
             the calling tenant's upstreams, checks for a duplicate-match conflict, and \
             persists the new route.",
        )
        .tag(ROUTES_TAG)
        .authenticated()
        .no_license_required()
        .json_request_no_desc::<RouteRequest>(openapi)
        .handler(route_handlers::create_route)
        .json_response_with_schema::<Route>(openapi, StatusCode::CREATED, "The created route")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/routes")
        .operation_id("oagw.routes.list")
        .summary("List routes")
        .description("Lists the calling tenant's own routes, honoring $filter, $select, $orderby, $top, and $skip.")
        .tag(ROUTES_TAG)
        .authenticated()
        .no_license_required()
        .query_param("$filter", false, "OData-style 'field eq value' filter clause")
        .query_param("$select", false, "Comma-separated list of fields to return")
        .query_param("$orderby", false, "'field' or 'field asc|desc'")
        .query_param_typed("$top", false, "Max results (default 50, max 100)", "integer")
        .query_param_typed("$skip", false, "Offset for pagination", "integer")
        .handler(route_handlers::list_routes)
        .json_response(StatusCode::OK, "The filtered, paginated list of routes")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/routes/{id}")
        .operation_id("oagw.routes.get")
        .summary("Get a route by id")
        .tag(ROUTES_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "The route's UUID")
        .handler(route_handlers::get_route)
        .json_response_with_schema::<Route>(openapi, StatusCode::OK, "The requested route")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::put("/oagw/v1/routes/{id}")
        .operation_id("oagw.routes.replace")
        .summary("Replace a route")
        .description("Full-field replacement; upstream_id and id are immutable.")
        .tag(ROUTES_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "The route's UUID")
        .json_request_no_desc::<RouteRequest>(openapi)
        .handler(route_handlers::replace_route)
        .json_response_with_schema::<Route>(openapi, StatusCode::OK, "The replaced route")
        .standard_errors(openapi)
        .register(router, openapi);

    OperationBuilder::delete("/oagw/v1/routes/{id}")
        .operation_id("oagw.routes.delete")
        .summary("Delete a route")
        .tag(ROUTES_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "The route's UUID")
        .handler(route_handlers::delete_route)
        .json_response(StatusCode::NO_CONTENT, "Deleted")
        .standard_errors(openapi)
        .register(router, openapi)
}
// @cpt-end:cpt-cf-oagw-dod-route-crud-endpoints:p1:inst-route-routes-register-01

/// Registers the `/oagw/v1/plugins` endpoints
/// (`cpt-cf-oagw-feature-plugin-management`). Deliberately registers no
/// `PUT`/`PATCH` route for `/oagw/v1/plugins/{id}`
/// (`cpt-cf-oagw-dod-plugin-no-replace`): plugin definitions are immutable
/// after creation.
// @cpt-begin:cpt-cf-oagw-dod-plugin-create:p1:inst-plugin-routes-register-01
fn register_plugin_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.plugins.create")
        .summary("Create a custom plugin")
        .description(
            "Validates plugin_type, name uniqueness, config_schema, and the declared phases \
             against the type's permitted set, then persists the new custom plugin.",
        )
        .tag(PLUGINS_TAG)
        .authenticated()
        .no_license_required()
        .json_request_no_desc::<PluginRequest>(openapi)
        .handler(plugin_handlers::create_plugin)
        .json_response_with_schema::<Plugin>(openapi, StatusCode::CREATED, "The created plugin")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/plugins")
        .operation_id("oagw.plugins.list")
        .summary("List custom plugins")
        .description(
            "Lists the calling tenant's own stored custom plugin definitions, honoring \
             $filter, $select, $orderby, $top, and $skip. Named built-in plugins are never \
             stored and never listed.",
        )
        .tag(PLUGINS_TAG)
        .authenticated()
        .no_license_required()
        .query_param(
            "$filter",
            false,
            "OData-style 'field eq value' filter clause",
        )
        .query_param("$select", false, "Comma-separated list of fields to return")
        .query_param("$orderby", false, "'field' or 'field asc|desc'")
        .query_param_typed(
            "$top",
            false,
            "Max results (default 50, max 100)",
            "integer",
        )
        .query_param_typed("$skip", false, "Offset for pagination", "integer")
        .handler(plugin_handlers::list_plugins)
        .json_response(StatusCode::OK, "The filtered, paginated list of plugins")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/plugins/{id}")
        .operation_id("oagw.plugins.get")
        .summary("Get a plugin by id")
        .description("Returns the plugin resource, excluding its stored source text.")
        .tag(PLUGINS_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "The plugin's GTS identifier")
        .handler(plugin_handlers::get_plugin)
        .json_response_with_schema::<Plugin>(openapi, StatusCode::OK, "The requested plugin")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/plugins/{id}/source")
        .operation_id("oagw.plugins.get_source")
        .summary("Get a plugin's stored source")
        .description(
            "Returns the stored source text of a UUID-backed custom plugin; a named \
             built-in identifier returns 404, since it carries no stored source.",
        )
        .tag(PLUGINS_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "The plugin's GTS identifier")
        .handler(plugin_handlers::get_plugin_source)
        .json_response_with_schema::<PluginSource>(
            openapi,
            StatusCode::OK,
            "The plugin's stored source text",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    OperationBuilder::delete("/oagw/v1/plugins/{id}")
        .operation_id("oagw.plugins.delete")
        .summary("Delete a plugin")
        .description(
            "Deletes an unreferenced plugin; rejected with 409 when the plugin is still \
             bound to any upstream or route.",
        )
        .tag(PLUGINS_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "The plugin's GTS identifier")
        .handler(plugin_handlers::delete_plugin)
        .json_response(StatusCode::NO_CONTENT, "Deleted")
        .standard_errors(openapi)
        .register(router, openapi)
}
// @cpt-end:cpt-cf-oagw-dod-plugin-create:p1:inst-plugin-routes-register-01

/// Registers the plain-HTTP proxy data-plane endpoints
/// (`cpt-cf-oagw-feature-http-proxy`, `cpt-cf-oagw-dod-proxy-endpoint-registration`).
///
/// A method-agnostic proxy route is a poor fit for `OperationBuilder`
/// (management endpoints all declare one fixed method each), so this uses
/// plain `axum::routing::any` instead, merged onto the same router as every
/// other route registered above.
// @cpt-begin:cpt-cf-oagw-dod-proxy-endpoint-registration:p1:inst-proxy-routes-register-01
fn register_proxy_routes(router: Router) -> Router {
    router
        .route("/oagw/v1/proxy/{alias}", any(proxy_handlers::proxy_handler))
        .route(
            "/oagw/v1/proxy/{alias}/{*path}",
            any(proxy_handlers::proxy_handler),
        )
}
// @cpt-end:cpt-cf-oagw-dod-proxy-endpoint-registration:p1:inst-proxy-routes-register-01

/// Fallback handler for any `/oagw/v1/...` path not claimed by a
/// downstream-registered route
/// (`cpt-cf-oagw-dod-unmounted-path-fallback`).
async fn route_not_found(uri: Uri) -> Response {
    // @cpt-begin:cpt-cf-oagw-dod-unmounted-path-fallback:p1:inst-mount-check-fallback-01
    let path = uri.path();
    OagwError::route_not_found(format!("no route registered for '{path}'"))
        .with_instance(path)
        .into_response()
    // @cpt-end:cpt-cf-oagw-dod-unmounted-path-fallback:p1:inst-mount-check-fallback-01
}
