//! REST route registration for the OAGW control plane and data plane.
//!
//! Paths are **gear-relative**: the host applies its own prefix/middleware and
//! exposes them as `/oagw/v1/…`, so nothing here may ever start with `/api`.
//!
//! | Operation | Path |
//! |---|---|
//! | Upstream CRUD (S1) | `POST/GET /oagw/v1/upstreams`, `GET/PUT/DELETE /oagw/v1/upstreams/{id}` |
//! | Route CRUD (S2) | `POST/GET /oagw/v1/routes`, `GET/PUT/DELETE /oagw/v1/routes/{id}` |
//! | Plugin CRUD (S2) | `POST/GET /oagw/v1/plugins`, `GET /oagw/v1/plugins/{id}`, `GET /oagw/v1/plugins/{id}/source`, `DELETE /oagw/v1/plugins/{id}` |
//! | Proxy data plane (S2) | `ANY /oagw/v1/proxy/{alias}`, `ANY /oagw/v1/proxy/{alias}/{*path_suffix}` |
//!
//! The proxy paths are registered for the five proxied methods (`GET`, `POST`,
//! `PUT`, `DELETE`, `PATCH`) plus `OPTIONS` for the CORS preflight (ADR-0004) —
//! all on one handler; axum merges the `MethodRouter`s of a path, so the six
//! registrations are one route. There is deliberately no
//! `PUT /oagw/v1/plugins/{id}`: a custom plugin is immutable (DESIGN "Plugin
//! Immutability").

use std::sync::Arc;

use axum::Router;
use axum::http::{Method, StatusCode};
use axum::routing::MethodRouter;
use toolkit::api::{OpenApiRegistry, OperationBuilder, ResponseSpec};

use crate::config::OagwConfig;
use crate::domain::control_plane::ControlPlane;
use crate::domain::model::{PluginSpec, RouteSpec, UpstreamSpec};
use crate::domain::proxy::DataPlaneService;

use super::handlers;

const TAG: &str = "OAGW";

/// Register the OAGW REST routes.
///
/// The control plane, the configuration and the data-plane service are attached
/// as router extensions, so every handler sees the same store and the same
/// shared upstream client. The registration is split per resource so that no
/// single function grows past the workspace function-length limit.
pub fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    plane: Arc<ControlPlane>,
    config: OagwConfig,
) -> Router {
    // The routes are registered on their own router so that the extensions and
    // the ADR-0007 error-source header apply to OAGW responses only, never to
    // the routes of the other gears merged into `router`.
    let oagw = Router::new();
    let oagw = register_upstream_routes(oagw, openapi);
    let oagw = register_route_routes(oagw, openapi);
    let oagw = register_plugin_routes(oagw, openapi);
    let oagw = register_proxy(oagw, openapi);

    let data_plane = Arc::new(DataPlaneService::new(Arc::clone(&plane), config));
    router.merge(
        oagw.layer(axum::Extension(data_plane))
            .layer(axum::Extension(plane))
            .layer(axum::Extension(config)),
    )
}

/// Register the upstream CRUD operations (S1).
fn register_upstream_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let oagw = router;
    let oagw = OperationBuilder::post("/oagw/v1/upstreams")
        .operation_id("oagw.create_upstream")
        .summary("Create an upstream")
        .description(
            "Create an upstream for the authenticated tenant. The alias is derived from the \
         endpoints (hostname-based endpoints always auto-derive it) or, for IP-based and \
         non-derivable endpoint sets, taken from the request; it is unique per tenant.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<UpstreamSpec>(openapi, "Upstream document")
        .handler(handlers::create_upstream)
        .json_response_with_schema::<UpstreamSpec>(openapi, StatusCode::CREATED, "Upstream created")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(oagw, openapi);

    let oagw = OperationBuilder::get("/oagw/v1/upstreams")
        .operation_id("oagw.list_upstreams")
        .summary("List upstreams")
        .description(
            "List the upstreams of the authenticated tenant, ordered by alias. `$top` (default \
         50, maximum 100) and `$skip` page through the result.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .query_param_typed(
            "$top",
            false,
            "Maximum number of returned upstreams",
            "integer",
        )
        .query_param_typed("$skip", false, "Number of upstreams to skip", "integer")
        .handler(handlers::list_upstreams)
        .json_array_response_with_schema::<UpstreamSpec>(
            openapi,
            StatusCode::OK,
            "Upstreams of the tenant",
        )
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(oagw, openapi);

    let oagw = OperationBuilder::get("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.get_upstream")
        .summary("Get an upstream")
        .description(
            "Fetch an upstream of the authenticated tenant. `{id}` is a UUID or the GTS \
         identifier `gts.cf.core.oagw.upstream.v1~{uuid}`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream UUID or GTS identifier")
        .handler(handlers::get_upstream)
        .json_response_with_schema::<UpstreamSpec>(openapi, StatusCode::OK, "Upstream document")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(oagw, openapi);

    let oagw = OperationBuilder::put("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.put_upstream")
        .summary("Replace an upstream")
        .description(
            "Replace an upstream of the authenticated tenant. The representation is replaced \
         whole and the alias is immutable: an endpoint change that would change the derived \
         alias is rejected (delete and re-create instead). A `PUT` never creates.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream UUID or GTS identifier")
        .json_request::<UpstreamSpec>(openapi, "Upstream document")
        .handler(handlers::put_upstream)
        .json_response_with_schema::<UpstreamSpec>(
            openapi,
            StatusCode::OK,
            "Updated upstream document",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(oagw, openapi);

    OperationBuilder::delete("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.delete_upstream")
        .summary("Delete an upstream")
        .description("Delete an upstream owned by the authenticated tenant.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream UUID or GTS identifier")
        .handler(handlers::delete_upstream)
        .no_content_response(StatusCode::NO_CONTENT, "Upstream deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(oagw, openapi)
}

/// Register the route CRUD operations (S2).
fn register_route_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let oagw = router;
    let oagw = OperationBuilder::post("/oagw/v1/routes")
        .operation_id("oagw.create_route")
        .summary("Create a route")
        .description(
            "Create a route of an upstream owned by the authenticated tenant. `upstream_id` must \
         reference an upstream of the tenant and no other route of that upstream may already \
         match the same path for an overlapping method set.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<RouteSpec>(openapi, "Route document")
        .handler(handlers::create_route)
        .json_response_with_schema::<RouteSpec>(openapi, StatusCode::CREATED, "Route created")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(oagw, openapi);

    let oagw = OperationBuilder::get("/oagw/v1/routes")
        .operation_id("oagw.list_routes")
        .summary("List routes")
        .description(
            "List the routes of the authenticated tenant, ordered by upstream and match path. \
         `$top` (default 50, maximum 100) and `$skip` page through the result.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .query_param_typed(
            "$top",
            false,
            "Maximum number of returned routes",
            "integer",
        )
        .query_param_typed("$skip", false, "Number of routes to skip", "integer")
        .handler(handlers::list_routes)
        .json_array_response_with_schema::<RouteSpec>(
            openapi,
            StatusCode::OK,
            "Routes of the tenant",
        )
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(oagw, openapi);

    let oagw = OperationBuilder::get("/oagw/v1/routes/{id}")
        .operation_id("oagw.get_route")
        .summary("Get a route")
        .description(
            "Fetch a route of the authenticated tenant. `{id}` is a UUID or the GTS identifier \
         `gts.cf.core.oagw.route.v1~{uuid}`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route UUID or GTS identifier")
        .handler(handlers::get_route)
        .json_response_with_schema::<RouteSpec>(openapi, StatusCode::OK, "Route document")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(oagw, openapi);

    let oagw = OperationBuilder::put("/oagw/v1/routes/{id}")
        .operation_id("oagw.put_route")
        .summary("Replace a route")
        .description(
            "Replace a route of the authenticated tenant. The representation is replaced whole, \
         `upstream_id` is immutable and a `PUT` never creates.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route UUID or GTS identifier")
        .json_request::<RouteSpec>(openapi, "Route document")
        .handler(handlers::put_route)
        .json_response_with_schema::<RouteSpec>(openapi, StatusCode::OK, "Updated route document")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(oagw, openapi);

    OperationBuilder::delete("/oagw/v1/routes/{id}")
        .operation_id("oagw.delete_route")
        .summary("Delete a route")
        .description("Delete a route owned by the authenticated tenant.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route UUID or GTS identifier")
        .handler(handlers::delete_route)
        .no_content_response(StatusCode::NO_CONTENT, "Route deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(oagw, openapi)
}

/// Register the plugin CRUD operations (S2).
fn register_plugin_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let oagw = router;
    let oagw = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.create_plugin")
        .summary("Register a plugin")
        .description(
            "Register a custom (Starlark) plugin for the authenticated tenant, or name a registry \
         plugin when the document carries no `source`. A registered plugin is immutable: it \
         is replaced by registering a new one and deleting the old.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<PluginSpec>(openapi, "Plugin document")
        .handler(handlers::create_plugin)
        .json_response_with_schema::<PluginSpec>(openapi, StatusCode::CREATED, "Plugin registered")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(oagw, openapi);

    let oagw = OperationBuilder::get("/oagw/v1/plugins")
        .operation_id("oagw.list_plugins")
        .summary("List plugins")
        .description(
            "List the plugins of the authenticated tenant, ordered by name. `$top` (default 50, \
         maximum 100) and `$skip` page through the result.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .query_param_typed(
            "$top",
            false,
            "Maximum number of returned plugins",
            "integer",
        )
        .query_param_typed("$skip", false, "Number of plugins to skip", "integer")
        .handler(handlers::list_plugins)
        .json_array_response_with_schema::<PluginSpec>(
            openapi,
            StatusCode::OK,
            "Plugins of the tenant",
        )
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(oagw, openapi);

    let oagw = OperationBuilder::get("/oagw/v1/plugins/{id}")
        .operation_id("oagw.get_plugin")
        .summary("Get a plugin")
        .description(
            "Fetch a plugin of the authenticated tenant. `{id}` is a UUID or the GTS identifier \
         `gts.cf.core.oagw.<kind>_plugin.v1~{uuid}`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin UUID or GTS identifier")
        .handler(handlers::get_plugin)
        .json_response_with_schema::<PluginSpec>(openapi, StatusCode::OK, "Plugin document")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(oagw, openapi);

    let oagw = OperationBuilder::get("/oagw/v1/plugins/{id}/source")
        .operation_id("oagw.get_plugin_source")
        .summary("Get the plugin source")
        .description("Return the Starlark source of a custom plugin as plain text.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin UUID or GTS identifier")
        .handler(handlers::get_plugin_source)
        .text_response(StatusCode::OK, "Plugin source", "text/plain")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(oagw, openapi);

    OperationBuilder::delete("/oagw/v1/plugins/{id}")
        .operation_id("oagw.delete_plugin")
        .summary("Delete a plugin")
        .description(
            "Delete a plugin of the authenticated tenant. The deletion is refused with a 409 \
         while an upstream or a route still binds the plugin.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin UUID or GTS identifier")
        .handler(handlers::delete_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "Plugin deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(oagw, openapi)
}

/// Register the two proxy paths for the five proxied methods.
///
/// Every method points at the same handler: the data plane decides per request
/// which route of the alias serves it. One `MethodRouter` per method keeps the
/// axum path merge free of overlapping method routes.
fn register_proxy(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    const PATHS: [&str; 2] = [
        "/oagw/v1/proxy/{alias}",
        "/oagw/v1/proxy/{alias}/{*path_suffix}",
    ];
    // `OPTIONS` is registered for the CORS preflight (ADR-0004): the data plane
    // answers it before it resolves anything, so it is not one of the proxied
    // methods and `ensure_supported_method` still refuses a plain `OPTIONS`.
    const METHODS: [(Method, &str, &str); 6] = [
        (
            Method::GET,
            "get",
            "Forward a GET request through the gateway",
        ),
        (
            Method::POST,
            "post",
            "Forward a POST request through the gateway",
        ),
        (
            Method::PUT,
            "put",
            "Forward a PUT request through the gateway",
        ),
        (
            Method::DELETE,
            "delete",
            "Forward a DELETE request through the gateway",
        ),
        (
            Method::PATCH,
            "patch",
            "Forward a PATCH request through the gateway",
        ),
        (
            Method::OPTIONS,
            "options",
            "Answer the CORS preflight of a proxy request",
        ),
    ];

    let mut router = router;
    for path in PATHS {
        for (method, name, summary) in METHODS {
            let Some(method_router) = proxy_method_router(&method) else {
                continue;
            };
            router = OperationBuilder::new(method, path)
                .operation_id(format!("oagw.proxy_{name}"))
                .summary(summary)
                .description(
                    "Forward a request to the upstream the proxy URL names. The alias selects the \
                     upstream, the suffix is matched against its routes and the optional \
                     `X-OAGW-Target-Host` header pins one endpoint of the pool. The upstream \
                     response is passed through unchanged (body streaming) and carries \
                     `X-OAGW-Error-Source: upstream`; the gateway reports a 413 for an oversized \
                     request body. A cross-origin request that the CORS policy of the upstream \
                     refuses is a 403, and a request that exhausts the rate limit of its upstream \
                     or route is a 429 with `Retry-After` and the `X-RateLimit-*` headers.",
                )
                .tag(TAG)
                .authenticated()
                .no_license_required()
                .path_param("alias", "Routing alias of the upstream")
                .method_router(method_router)
                .response(ResponseSpec {
                    status: StatusCode::OK.as_u16(),
                    content_type: "*/*",
                    description: "The upstream response, passed through unchanged".to_owned(),
                    schema: None,
                })
                .error_400(openapi)
                .error_401(openapi)
                .error_403(openapi)
                .error_404(openapi)
                .problem_response(
                    openapi,
                    StatusCode::TOO_MANY_REQUESTS,
                    "Rate Limit Exceeded",
                )
                .problem_response(openapi, StatusCode::PAYLOAD_TOO_LARGE, "Payload Too Large")
                .problem_response(openapi, StatusCode::BAD_GATEWAY, "Bad Gateway")
                .problem_response(
                    openapi,
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Service Unavailable",
                )
                .problem_response(openapi, StatusCode::GATEWAY_TIMEOUT, "Gateway Timeout")
                .error_500(openapi)
                .register(router, openapi);
        }
    }
    router
}

/// The single-method router of `method`, or `None` when the data plane does not
/// proxy it. `OPTIONS` is registered for the CORS preflight (ADR-0004).
fn proxy_method_router(method: &Method) -> Option<MethodRouter> {
    match *method {
        Method::GET => Some(axum::routing::get(handlers::proxy)),
        Method::POST => Some(axum::routing::post(handlers::proxy)),
        Method::PUT => Some(axum::routing::put(handlers::proxy)),
        Method::DELETE => Some(axum::routing::delete(handlers::proxy)),
        Method::PATCH => Some(axum::routing::patch(handlers::proxy)),
        Method::OPTIONS => Some(axum::routing::options(handlers::proxy)),
        _ => None,
    }
}

#[cfg(test)]
#[path = "routes_tests.rs"]
mod tests;
