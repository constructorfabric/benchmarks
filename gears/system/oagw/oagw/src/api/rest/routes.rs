//! REST route registration for the OAGW gear.
//!
//! The sub-router is built **once** at the canonical `/oagw/v1/...` paths and
//! then mounted twice by [`crate::gear`]: merged directly onto the platform
//! router (so `/oagw/v1/...` resolves) and nested under `/api`
//! (`/api/oagw/v1/...`, the `DESIGN.md` §3.3 contract paths).
//!
//! Every endpoint — Control Plane and Data Plane — is registered through
//! [`OperationBuilder::register`], so the OpenAPI document and the axum router
//! cannot drift apart. The only exceptions are `HEAD` and `OPTIONS` on the
//! proxy paths, which are served but not declared as separate operations: the
//! toolkit OpenAPI registry maps any method other than `GET`, `POST`, `PUT`,
//! `PATCH` and `DELETE` onto the `get` entry of the path item, so declaring
//! them would silently overwrite the documented `GET` operation.

use axum::Router;
use axum::http::{Method, StatusCode};
use toolkit::api::{OpenApiRegistry, OperationBuilder};

use super::handlers::{self, ControlPlane};
use super::proxy::{self, DataPlane};

/// OpenAPI tag for the Control Plane endpoints.
const CONTROL_TAG: &str = "Outbound API Gateway Control Plane";

/// OpenAPI tag for the Data Plane endpoints.
const DATA_TAG: &str = "Outbound API Gateway Data Plane";

/// Description shared by every proxy operation.
const PROXY_DESCRIPTION: &str = "Resolve `{alias}` through the tenant chain, run the effective plugin chain and relay the \
     request to the selected endpoint. Any HTTP method is accepted; requests carrying an \
     `Upgrade` token are relayed as a raw bidirectional stream. `HEAD` and `OPTIONS` are relayed \
     too but are not declared as separate operations (see the module documentation).";

/// The methods declared as OpenAPI operations on a proxy path.
const PROXY_METHODS: [(Method, &str); 5] = [
    (Method::GET, "oagw.proxy_get"),
    (Method::POST, "oagw.proxy_post"),
    (Method::PUT, "oagw.proxy_put"),
    (Method::PATCH, "oagw.proxy_patch"),
    (Method::DELETE, "oagw.proxy_delete"),
];

/// Builds the OAGW sub-router, registered in the OpenAPI document exactly once.
pub fn build_router(
    control_plane: ControlPlane,
    data_plane: DataPlane,
    openapi: &dyn OpenApiRegistry,
) -> Router {
    let router = Router::new();

    let router = upstream_endpoints(router, openapi);
    let router = route_endpoints(router, openapi);
    let router = plugin_endpoints(router, openapi);
    let router = data_plane_endpoints(router, openapi);

    router
        .layer(axum::Extension(control_plane))
        .layer(axum::Extension(data_plane))
}

/// Registers the upstream endpoints.
fn upstream_endpoints(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::post("/oagw/v1/upstreams")
        .operation_id("oagw.create_upstream")
        .summary("Create an upstream")
        .description("Create an upstream for the authenticated tenant; the alias is derived from hostname endpoints when omitted.")
        .tag(CONTROL_TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::create_upstream)
        .json_response(StatusCode::CREATED, "Upstream created")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_409(openapi)
        .error_415(openapi)
        .error_422(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/upstreams")
        .operation_id("oagw.list_upstreams")
        .summary("List upstreams")
        .description(
            "List the upstreams owned by the calling tenant, with OData paging and filtering.",
        )
        .tag(CONTROL_TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::list_upstreams)
        .json_response(StatusCode::OK, "Upstream collection")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.get_upstream")
        .summary("Get an upstream")
        .description("Fetch an upstream owned by the calling tenant.")
        .tag(CONTROL_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream GTS identifier or UUID")
        .handler(handlers::get_upstream)
        .json_response(StatusCode::OK, "Upstream")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::put("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.replace_upstream")
        .summary("Replace an upstream")
        .description(
            "Replace an upstream owned by the calling tenant; omitted optional fields are cleared.",
        )
        .tag(CONTROL_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream GTS identifier or UUID")
        .handler(handlers::replace_upstream)
        .json_response(StatusCode::OK, "Upstream replaced")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_415(openapi)
        .error_422(openapi)
        .error_500(openapi)
        .register(router, openapi);

    OperationBuilder::delete("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.delete_upstream")
        .summary("Delete an upstream")
        .description(
            "Delete an upstream owned by the calling tenant. The operation fails with `409` while \
             any route still references the upstream — delete those routes first. The upstream is \
             never deleted together with its routes.",
        )
        .tag(CONTROL_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream GTS identifier or UUID")
        .handler(handlers::delete_upstream)
        .no_content_response(StatusCode::NO_CONTENT, "Upstream deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi)
}

/// Registers the route endpoints.
fn route_endpoints(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::post("/oagw/v1/routes")
        .operation_id("oagw.create_route")
        .summary("Create a route")
        .description("Create a route for an upstream owned by the calling tenant.")
        .tag(CONTROL_TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::create_route)
        .json_response(StatusCode::CREATED, "Route created")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_415(openapi)
        .error_422(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/routes")
        .operation_id("oagw.list_routes")
        .summary("List routes")
        .description(
            "List the routes owned by the calling tenant, with OData paging and filtering.",
        )
        .tag(CONTROL_TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::list_routes)
        .json_response(StatusCode::OK, "Route collection")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/routes/{id}")
        .operation_id("oagw.get_route")
        .summary("Get a route")
        .description("Fetch a route owned by the calling tenant.")
        .tag(CONTROL_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route GTS identifier or UUID")
        .handler(handlers::get_route)
        .json_response(StatusCode::OK, "Route")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::put("/oagw/v1/routes/{id}")
        .operation_id("oagw.replace_route")
        .summary("Replace a route")
        .description("Replace a route owned by the calling tenant; `upstream_id` is immutable.")
        .tag(CONTROL_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route GTS identifier or UUID")
        .handler(handlers::replace_route)
        .json_response(StatusCode::OK, "Route replaced")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_415(openapi)
        .error_422(openapi)
        .error_500(openapi)
        .register(router, openapi);

    OperationBuilder::delete("/oagw/v1/routes/{id}")
        .operation_id("oagw.delete_route")
        .summary("Delete a route")
        .description("Delete a route owned by the calling tenant.")
        .tag(CONTROL_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route GTS identifier or UUID")
        .handler(handlers::delete_route)
        .no_content_response(StatusCode::NO_CONTENT, "Route deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi)
}

/// Registers the plugin endpoints.
///
/// `GET /plugins/{id}`, `GET /plugins/{id}/source` and `DELETE /plugins/{id}`
/// can only fail with `503` `PluginNotFound` — the `DESIGN.md` §3.3 contract
/// table reports an unresolvable plugin reference as service unavailable — so
/// they declare `503` and never `404`. Upstream and route operations keep
/// their `404`.
fn plugin_endpoints(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.create_plugin")
        .summary("Create a plugin")
        .description("Register a tenant-defined Starlark plugin.")
        .tag(CONTROL_TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::create_plugin)
        .json_response(StatusCode::CREATED, "Plugin created")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_409(openapi)
        .error_415(openapi)
        .error_422(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/plugins")
        .operation_id("oagw.list_plugins")
        .summary("List plugins")
        .description(
            "List the plugins owned by the calling tenant, with OData paging and filtering \
             (`$filter`, `$select`, `$orderby`, `$top`, `$skip`) and the `type` / `plugin_type` \
             shorthand (for example `?type=guard`).",
        )
        .tag(CONTROL_TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::list_plugins)
        .json_response(StatusCode::OK, "Plugin collection")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/plugins/{id}")
        .operation_id("oagw.get_plugin")
        .summary("Get a plugin")
        .description("Fetch a plugin owned by the calling tenant.")
        .tag(CONTROL_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin GTS identifier or UUID")
        .handler(handlers::get_plugin)
        .json_response(StatusCode::OK, "Plugin")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_503(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/plugins/{id}/source")
        .operation_id("oagw.get_plugin_source")
        .summary("Get the Starlark source of a plugin")
        .description("Return the Starlark source of a plugin owned by the calling tenant.")
        .tag(CONTROL_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin GTS identifier or UUID")
        .handler(handlers::get_plugin_source)
        .json_response(StatusCode::OK, "Plugin source")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_503(openapi)
        .error_500(openapi)
        .register(router, openapi);

    OperationBuilder::delete("/oagw/v1/plugins/{id}")
        .operation_id("oagw.delete_plugin")
        .summary("Delete a plugin")
        .description(
            "Delete a plugin owned by the calling tenant; fails with 409 when still referenced.",
        )
        .tag(CONTROL_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin GTS identifier or UUID")
        .handler(handlers::delete_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "Plugin deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_409(openapi)
        .error_503(openapi)
        .error_500(openapi)
        .register(router, openapi)
}

/// Registers the Data Plane endpoints.
///
/// The proxy path serves every method (including `HEAD` and `OPTIONS`, and
/// `Upgrade` requests on `GET`); SSE streams are relayed through the same
/// path. `/oagw/v1/ws/{alias}/{*path}` is the documented alias of the wildcard
/// proxy path used by WebSocket clients, and is registered as an operation so
/// that the OpenAPI document lists it.
fn data_plane_endpoints(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = proxy_operations(
        router,
        openapi,
        "/oagw/v1/proxy/{alias}",
        proxy::relay_root,
        false,
    );
    let router = proxy_operations(
        router,
        openapi,
        "/oagw/v1/proxy/{alias}/{*path}",
        proxy::relay,
        true,
    );
    relay_operations(
        router,
        openapi,
        "/oagw/v1/ws/{alias}/{*path}",
        "oagw.ws_relay",
    )
}

/// Registers every documented proxy method on one path through the canonical
/// [`OperationBuilder::register`] path, then attaches `HEAD` and `OPTIONS` to
/// the same axum path (see the module documentation).
fn proxy_operations<H, T>(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    path: &str,
    handler: H,
    wildcard: bool,
) -> Router
where
    H: axum::handler::Handler<T, ()> + Clone + Send + 'static,
    T: 'static,
{
    let mut router = router;
    for (method, operation_id) in PROXY_METHODS {
        let builder = OperationBuilder::new(method, path)
            .operation_id(operation_id)
            .summary("Proxy a request to an upstream")
            .description(PROXY_DESCRIPTION)
            .tag(DATA_TAG)
            .exposed()
            .authenticated()
            .no_license_required()
            .path_param("alias", "Upstream alias");
        let builder = if wildcard {
            builder.path_param("path", "Path suffix appended to the matched route prefix")
        } else {
            builder
        };
        router = builder
            .handler(handler.clone())
            .json_response(StatusCode::OK, "Relayed upstream response")
            .error_400(openapi)
            .error_401(openapi)
            .error_403(openapi)
            .error_404(openapi)
            .problem_response(openapi, StatusCode::PAYLOAD_TOO_LARGE, "Payload Too Large")
            .error_429(openapi)
            .error_500(openapi)
            .error_502(openapi)
            .error_503(openapi)
            .error_504(openapi)
            .register(router, openapi);
    }
    router.route(path, axum::routing::head(handler.clone()).options(handler))
}

/// Registers the WebSocket relay path for the methods it serves.
fn relay_operations(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    path: &str,
    operation_id: &str,
) -> Router {
    let router = OperationBuilder::get(path)
        .operation_id(format!("{operation_id}_get"))
        .summary("Relay a request on the WebSocket path")
        .description(
            "Relay `{METHOD} /ws/{alias}/{path_suffix}` exactly like the proxy path. This is an \
             explicitly supported alias of `/proxy/{alias}/{*path}` kept for WebSocket clients \
             that address the relay through the `ws` prefix; protocol upgrades are detected by \
             header, so the same handler serves the raw bidirectional stream.",
        )
        .tag(DATA_TAG)
        .exposed()
        .authenticated()
        .no_license_required()
        .path_param("alias", "Upstream alias")
        .path_param("path", "Path suffix appended to the matched route prefix")
        .handler(proxy::relay)
        .json_response(StatusCode::OK, "Relayed upstream response")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .problem_response(openapi, StatusCode::PAYLOAD_TOO_LARGE, "Payload Too Large")
        .error_429(openapi)
        .error_500(openapi)
        .error_502(openapi)
        .error_503(openapi)
        .error_504(openapi)
        .register(router, openapi);

    OperationBuilder::post(path)
        .operation_id(format!("{operation_id}_post"))
        .summary("Relay a request on the WebSocket path")
        .description(
            "Relay `POST /ws/{alias}/{path_suffix}` exactly like the proxy path. Protocol \
             upgrades are detected by header, so the path also carries upgraded connections.",
        )
        .tag(DATA_TAG)
        .exposed()
        .authenticated()
        .no_license_required()
        .path_param("alias", "Upstream alias")
        .path_param("path", "Path suffix appended to the matched route prefix")
        .handler(proxy::relay)
        .json_response(StatusCode::OK, "Relayed upstream response")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .problem_response(openapi, StatusCode::PAYLOAD_TOO_LARGE, "Payload Too Large")
        .error_429(openapi)
        .error_500(openapi)
        .error_502(openapi)
        .error_503(openapi)
        .error_504(openapi)
        .register(router, openapi)
}
