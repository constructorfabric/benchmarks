//! Route registration for the OAGW management + proxy API (DESIGN §3.3).
//!
//! Paths are gear-relative (`/oagw/v1/...`); the host exposes them at the
//! same location. Every route requires a bearer token: management routes for
//! their CRUD permissions, the proxy route so the caller's `SecurityContext`
//! reaches the data plane, which PDP-gates invocation via the
//! `cf.core.oagw.proxy.v1~:invoke` permission (DESIGN §3.2).

use std::sync::Arc;

use axum::Router;
use axum::routing::any;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationBuilder;

use super::{dto, handlers};
use crate::domain::services::control_plane::ControlPlaneService;
use crate::infra::proxy::ProxyService;

const API_TAG: &str = "OAGW";

/// Register all OAGW routes onto `router`.
///
/// # Errors
/// Route registration is infallible.
#[allow(clippy::needless_pass_by_value)]
pub fn register_routes(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
    control: Arc<ControlPlaneService>,
    proxy: Arc<ProxyService>,
) -> Router {
    router = register_upstream_routes(router, openapi);
    router = register_route_routes(router, openapi);
    router = register_plugin_routes(router, openapi);
    router = register_proxy_route(router, openapi);

    router = router
        .layer(axum::Extension(control))
        .layer(axum::Extension(proxy));

    router
}

fn register_upstream_routes(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
) -> Router {
    router = OperationBuilder::post("/oagw/v1/upstreams")
        .operation_id("oagw.create_upstream")
        .summary("Create an upstream")
        .description("Register an outbound upstream service and derive/bind its routing alias.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<dto::UpstreamRequest>(openapi, "Upstream definition")
        .handler(handlers::create_upstream)
        .json_response(http::StatusCode::CREATED, "Created upstream")
        .error_400(openapi)
        .error_403(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams")
        .operation_id("oagw.list_upstreams")
        .summary("List upstreams")
        .description("List the calling tenant's upstreams with OData filter/pagination.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .query_param("$top", false, "Max results (default 50, max 100)")
        .query_param("$skip", false, "Offset")
        .query_param("$filter", false, "OData equality filter, e.g. alias eq 'x'")
        .handler(handlers::list_upstreams)
        .json_response(http::StatusCode::OK, "List of upstreams")
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.get_upstream")
        .summary("Get an upstream")
        .description("Fetch an upstream owned by the calling tenant by id.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream UUID")
        .handler(handlers::get_upstream)
        .json_response(http::StatusCode::OK, "Upstream found")
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.update_upstream")
        .summary("Replace an upstream")
        .description("Full replacement via PUT; the alias is immutable.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream UUID")
        .json_request::<dto::UpstreamRequest>(openapi, "Replacement upstream definition")
        .handler(handlers::update_upstream)
        .json_response(http::StatusCode::OK, "Updated upstream")
        .error_400(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.delete_upstream")
        .summary("Delete an upstream")
        .description("Delete an upstream; 409 while routes still reference it.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream UUID")
        .handler(handlers::delete_upstream)
        .no_content_response(http::StatusCode::NO_CONTENT, "Upstream deleted")
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router
}

fn register_route_routes(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    router = OperationBuilder::post("/oagw/v1/routes")
        .operation_id("oagw.create_route")
        .summary("Create a route")
        .description("Bind inbound traffic to an upstream owned by the calling tenant.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<dto::RouteRequest>(openapi, "Route definition")
        .handler(handlers::create_route)
        .json_response(http::StatusCode::CREATED, "Created route")
        .error_400(openapi)
        .error_403(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes")
        .operation_id("oagw.list_routes")
        .summary("List routes")
        .description("List the calling tenant's routes with OData filter/pagination.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .query_param("$top", false, "Max results (default 50, max 100)")
        .query_param("$skip", false, "Offset")
        .query_param("$filter", false, "OData equality filter, e.g. upstream_id eq 'x'")
        .handler(handlers::list_routes)
        .json_response(http::StatusCode::OK, "List of routes")
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes/{id}")
        .operation_id("oagw.get_route")
        .summary("Get a route")
        .description("Fetch a route owned by the calling tenant by id.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route UUID")
        .handler(handlers::get_route)
        .json_response(http::StatusCode::OK, "Route found")
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/routes/{id}")
        .operation_id("oagw.update_route")
        .summary("Replace a route")
        .description("Full replacement via PUT; `upstream_id` is immutable.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route UUID")
        .json_request::<dto::RouteUpdateRequest>(openapi, "Replacement route definition")
        .handler(handlers::update_route)
        .json_response(http::StatusCode::OK, "Updated route")
        .error_400(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/routes/{id}")
        .operation_id("oagw.delete_route")
        .summary("Delete a route")
        .description("Delete a route owned by the calling tenant.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route UUID")
        .handler(handlers::delete_route)
        .no_content_response(http::StatusCode::NO_CONTENT, "Route deleted")
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router
}

fn register_plugin_routes(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    router = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.create_plugin")
        .summary("Create a custom plugin")
        .description("Define a custom plugin parameterizing a builtin implementation.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<dto::PluginRequest>(openapi, "Plugin definition")
        .handler(handlers::create_plugin)
        .json_response(http::StatusCode::CREATED, "Created plugin")
        .error_400(openapi)
        .error_403(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins")
        .operation_id("oagw.list_plugins")
        .summary("List custom plugins")
        .description("List the calling tenant's plugins with OData filter/pagination.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .query_param("$top", false, "Max results (default 50, max 100)")
        .query_param("$skip", false, "Offset")
        .query_param("$filter", false, "OData equality filter, e.g. type eq 'guard'")
        .handler(handlers::list_plugins)
        .json_response(http::StatusCode::OK, "List of plugins")
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{id}")
        .operation_id("oagw.get_plugin")
        .summary("Get a custom plugin")
        .description("Fetch a custom plugin owned by the calling tenant by id.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin UUID")
        .handler(handlers::get_plugin)
        .json_response(http::StatusCode::OK, "Plugin found")
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/plugins/{id}")
        .operation_id("oagw.delete_plugin")
        .summary("Delete a custom plugin")
        .description("Delete a custom plugin; 409 while upstreams/routes still reference it.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin UUID")
        .handler(handlers::delete_plugin)
        .no_content_response(http::StatusCode::NO_CONTENT, "Plugin deleted")
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{id}/source")
        .operation_id("oagw.get_plugin_source")
        .summary("Get plugin source")
        .description("Retrieve the source representation of a custom plugin.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin UUID")
        .handler(handlers::get_plugin_source)
        .text_response(http::StatusCode::OK, "Plugin source", "text/plain")
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router
}

fn register_proxy_route(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    // The proxy accepts every HTTP method on the alias path. `OperationBuilder`
    // has no "any" constructor, so we compose an `any(...)` MethodRouter and
    // attach it; the OpenAPI spec records a `GET` operation for the path.
    router = OperationBuilder::get("/oagw/v1/proxy/{alias}/{*suffix}")
        .operation_id("oagw.proxy")
        .summary("Proxy a request to an upstream")
        .description("Forward {METHOD} to the upstream bound to `alias`; the path suffix is appended to the upstream route.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("alias", "Upstream routing alias")
        .path_param("suffix", "Path suffix appended to the matched route")
        // Pin the router state to `()` so `any` can resolve before the
        // builder is registered onto `Router<()>`.
        .method_router(any::<_, _, ()>(handlers::proxy))
        .json_response(http::StatusCode::OK, "Upstream response")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_429(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router
}
