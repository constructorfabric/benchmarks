// Created: 2026-08-31 by Constructor Tech
//! Route registration (DESIGN §3.3 endpoint table and §3.5 proxy flow, with
//! the gear-relative `/oagw/v1` prefix of the deployment).
//!
//! The management routes live on a sub-router that carries the control-plane
//! service, the configured request-body limit and the OAGW problem
//! middleware. The proxy data plane is registered on its **own** sub-router:
//! it buffers bodies itself, streams responses and must therefore carry
//! neither `enforce_body_limit` (which rejects a declared body up front) nor
//! `enrich_problem_response` (which buffers a body to decorate it).

use std::sync::Arc;

use axum::Router;
use http::Method;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::{OperationBuilder, ResponseSpec};

use crate::api::dto::{
    CreatePluginRequest, CreateRouteRequest, CreateUpstreamRequest, PluginDto, RouteDto,
    UpdateRouteRequest, UpdateUpstreamRequest, UpstreamDto,
};
use crate::api::handlers;
use crate::domain::proxy::service::ProxyService;
use crate::domain::service::OagwService;

const API_TAG: &str = "Outbound API Gateway";

/// Paths of the proxy data plane (gear-relative, no `/api` prefix).
const PROXY_ALIAS_PATH: &str = "/oagw/v1/proxy/{alias}";
const PROXY_SUFFIX_PATH: &str = "/oagw/v1/proxy/{alias}/{*path_suffix}";

/// Methods a proxy request can use. `TRACE` and `CONNECT` are not proxied.
const PROXY_METHODS: [Method; 7] = [
    Method::GET,
    Method::POST,
    Method::PUT,
    Method::DELETE,
    Method::PATCH,
    Method::HEAD,
    Method::OPTIONS,
];

/// Register the management endpoints and the proxy data plane of the gear.
#[allow(clippy::needless_pass_by_value)]
pub fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    service: Arc<OagwService>,
) -> Router {
    let body_limit = crate::api::extract::BodyLimit(service.policy().max_body_bytes);
    let oagw = register_management_routes(Router::new(), openapi)
        .layer(axum::middleware::from_fn(
            crate::api::error::enrich_problem_response,
        ))
        .layer(axum::middleware::from_fn(
            crate::api::error::enforce_body_limit,
        ))
        .layer(axum::Extension(body_limit))
        .layer(axum::Extension(service));
    router.merge(oagw)
}

/// Register the proxy data plane on its own sub-router.
///
/// The data plane is wired separately from [`register_routes`] because it
/// needs a different `Extension` (the [`ProxyService`]) and no management
/// middleware.
pub fn register_data_plane(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    proxy: Arc<ProxyService>,
) -> Router {
    let data_plane = register_proxy_routes(Router::new(), openapi).layer(axum::Extension(proxy));
    router.merge(data_plane)
}

fn register_management_routes(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    router = register_upstream_routes(router, openapi);
    router = register_route_routes(router, openapi);
    router = register_plugin_routes(router, openapi);
    router
}

fn register_upstream_routes(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    router = OperationBuilder::post("/oagw/v1/upstreams")
        .operation_id("oagw.create_upstream")
        .summary("Create an upstream")
        .description(
            "Create an outbound upstream. The alias is derived from the endpoint pool unless \
             the pool requires an explicit one (IP-based endpoints).",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<CreateUpstreamRequest>(openapi, "Upstream creation payload")
        .handler(handlers::create_upstream)
        .json_response_with_schema::<UpstreamDto>(
            openapi,
            http::StatusCode::CREATED,
            "Upstream created",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams")
        .operation_id("oagw.list_upstreams")
        .summary("List upstreams")
        .description("List upstreams of the calling tenant")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .query_param(
            "$filter",
            false,
            "OData filter expression (field eq 'value')",
        )
        .query_param("$select", false, "Fields to return")
        .query_param("$orderby", false, "Sort order")
        .query_param_typed(
            "$top",
            false,
            "Maximum number of results (default 50, max 100)",
            "integer",
        )
        .query_param_typed("$skip", false, "Number of results to skip", "integer")
        .handler(handlers::list_upstreams)
        .json_response_with_schema::<UpstreamDto>(openapi, http::StatusCode::OK, "Upstream page")
        .error_400(openapi)
        .error_401(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.get_upstream")
        .summary("Get an upstream")
        .description("Read one upstream by UUID or GTS identifier")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream UUID or GTS identifier")
        .handler(handlers::get_upstream)
        .json_response_with_schema::<UpstreamDto>(openapi, http::StatusCode::OK, "Upstream found")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.replace_upstream")
        .summary("Replace an upstream")
        .description("Full replacement; the alias is immutable for hostname pools")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream UUID or GTS identifier")
        .json_request::<UpdateUpstreamRequest>(openapi, "Upstream replacement payload")
        .handler(handlers::update_upstream)
        .json_response_with_schema::<UpstreamDto>(
            openapi,
            http::StatusCode::OK,
            "Upstream replaced",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.delete_upstream")
        .summary("Delete an upstream")
        .description("Delete an upstream and cascade-delete its routes")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream UUID or GTS identifier")
        .handler(handlers::delete_upstream)
        .no_content_response(http::StatusCode::NO_CONTENT, "Upstream deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router
}

fn register_route_routes(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    router = OperationBuilder::post("/oagw/v1/routes")
        .operation_id("oagw.create_route")
        .summary("Create a route")
        .description("Create a route bound to an upstream of the calling tenant")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<CreateRouteRequest>(openapi, "Route creation payload")
        .handler(handlers::create_route)
        .json_response_with_schema::<RouteDto>(openapi, http::StatusCode::CREATED, "Route created")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes")
        .operation_id("oagw.list_routes")
        .summary("List routes")
        .description("List routes of the calling tenant")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .query_param(
            "$filter",
            false,
            "OData filter expression (field eq 'value')",
        )
        .query_param("$select", false, "Fields to return")
        .query_param("$orderby", false, "Sort order")
        .query_param_typed(
            "$top",
            false,
            "Maximum number of results (default 50, max 100)",
            "integer",
        )
        .query_param_typed("$skip", false, "Number of results to skip", "integer")
        .handler(handlers::list_routes)
        .json_response_with_schema::<RouteDto>(openapi, http::StatusCode::OK, "Route page")
        .error_400(openapi)
        .error_401(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes/{id}")
        .operation_id("oagw.get_route")
        .summary("Get a route")
        .description("Read one route by UUID or GTS identifier")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route UUID or GTS identifier")
        .handler(handlers::get_route)
        .json_response_with_schema::<RouteDto>(openapi, http::StatusCode::OK, "Route found")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/routes/{id}")
        .operation_id("oagw.replace_route")
        .summary("Replace a route")
        .description("Full replacement; `upstream_id` is immutable")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route UUID or GTS identifier")
        .json_request::<UpdateRouteRequest>(openapi, "Route replacement payload")
        .handler(handlers::update_route)
        .json_response_with_schema::<RouteDto>(openapi, http::StatusCode::OK, "Route replaced")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/routes/{id}")
        .operation_id("oagw.delete_route")
        .summary("Delete a route")
        .description("Delete a route by UUID or GTS identifier")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route UUID or GTS identifier")
        .handler(handlers::delete_route)
        .no_content_response(http::StatusCode::NO_CONTENT, "Route deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router
}

fn register_plugin_routes(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    router = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.create_plugin")
        .summary("Create a custom plugin")
        .description("Create a tenant-defined Starlark plugin; plugins are immutable")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<CreatePluginRequest>(openapi, "Plugin definition")
        .handler(handlers::create_plugin)
        .json_response_with_schema::<PluginDto>(
            openapi,
            http::StatusCode::CREATED,
            "Plugin created",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins")
        .operation_id("oagw.list_plugins")
        .summary("List custom plugins")
        .description("List custom plugins of the calling tenant")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .query_param(
            "$filter",
            false,
            "OData filter expression (field eq 'value')",
        )
        .query_param("$select", false, "Fields to return")
        .query_param("$orderby", false, "Sort order")
        .query_param_typed(
            "$top",
            false,
            "Maximum number of results (default 50, max 100)",
            "integer",
        )
        .query_param_typed("$skip", false, "Number of results to skip", "integer")
        .handler(handlers::list_plugins)
        .json_response_with_schema::<PluginDto>(openapi, http::StatusCode::OK, "Plugin page")
        .error_400(openapi)
        .error_401(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{id}")
        .operation_id("oagw.get_plugin")
        .summary("Get a custom plugin")
        .description("Read one custom plugin by UUID or GTS identifier")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin UUID or GTS identifier")
        .handler(handlers::get_plugin)
        .json_response_with_schema::<PluginDto>(openapi, http::StatusCode::OK, "Plugin found")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{id}/source")
        .operation_id("oagw.get_plugin_source")
        .summary("Get the Starlark source of a plugin")
        .description("Return the plugin source as text/plain")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin UUID or GTS identifier")
        .handler(handlers::get_plugin_source)
        .response(ResponseSpec {
            status: http::StatusCode::OK.as_u16(),
            content_type: "text/plain",
            description: "Starlark plugin source".to_owned(),
            schema: None,
        })
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/plugins/{id}")
        .operation_id("oagw.delete_plugin")
        .summary("Delete a custom plugin")
        .description("Delete a custom plugin; 409 while an upstream or route still binds it")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin UUID or GTS identifier")
        .handler(handlers::delete_plugin)
        .no_content_response(http::StatusCode::NO_CONTENT, "Plugin deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router
}

/// Register both proxy paths for every proxied method.
///
/// A proxy accepts any method on one path, so the axum router is composed per
/// method (`OperationBuilder::handler` only knows the five schema methods) and
/// the `OpenAPI` document declares one operation per method and path. The
/// fallback for the methods the data plane does not register is attached once
/// per path afterwards: axum refuses to merge two method routers that both
/// carry one.
fn register_proxy_routes(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    for method in PROXY_METHODS {
        let label = method.as_str().to_ascii_lowercase();
        let alias_router = proxy_method_router(method.clone());
        let suffix_router = proxy_method_router(method.clone());

        router = OperationBuilder::new(method.clone(), PROXY_ALIAS_PATH)
            .operation_id(format!("oagw.proxy_{label}"))
            .summary("Proxy a request to an upstream alias")
            .description(
                "Resolve the alias across the tenant chain, match a route and forward the \
                 request to the selected endpoint. The response is the upstream response, \
                 marked `X-OAGW-Error-Source: upstream`.",
            )
            .tag(API_TAG)
            .authenticated()
            .no_license_required()
            .path_param(
                "alias",
                "Upstream alias, or `alias:port` for a shared suffix",
            )
            .method_router(alias_router)
            .response(passthrough_response())
            .problem_response(
                openapi,
                http::StatusCode::BAD_REQUEST,
                "A guard rule rejected the request",
            )
            .problem_response(openapi, http::StatusCode::UNAUTHORIZED, "Unauthorized")
            .problem_response(
                openapi,
                http::StatusCode::NOT_FOUND,
                "Unknown alias or no matching route",
            )
            .problem_response(
                openapi,
                http::StatusCode::PAYLOAD_TOO_LARGE,
                "Body exceeds the configured limit",
            )
            .problem_response(
                openapi,
                http::StatusCode::BAD_GATEWAY,
                "Upstream protocol failure",
            )
            .problem_response(
                openapi,
                http::StatusCode::SERVICE_UNAVAILABLE,
                "Upstream unavailable",
            )
            .problem_response(
                openapi,
                http::StatusCode::GATEWAY_TIMEOUT,
                "Upstream timed out",
            )
            .register(router, openapi);

        router = OperationBuilder::new(method, PROXY_SUFFIX_PATH)
            .operation_id(format!("oagw.proxy_{label}_suffix"))
            .summary("Proxy a request with a path suffix")
            .description(
                "Same as the alias-root proxy operation, with the client path behind the \
                 alias appended to the matched route path.",
            )
            .tag(API_TAG)
            .authenticated()
            .no_license_required()
            .path_param(
                "alias",
                "Upstream alias, or `alias:port` for a shared suffix",
            )
            .path_param("path_suffix", "Client path behind the alias")
            .method_router(suffix_router)
            .response(passthrough_response())
            .problem_response(
                openapi,
                http::StatusCode::BAD_REQUEST,
                "A guard rule rejected the request",
            )
            .problem_response(openapi, http::StatusCode::UNAUTHORIZED, "Unauthorized")
            .problem_response(
                openapi,
                http::StatusCode::NOT_FOUND,
                "Unknown alias or no matching route",
            )
            .problem_response(
                openapi,
                http::StatusCode::PAYLOAD_TOO_LARGE,
                "Body exceeds the configured limit",
            )
            .problem_response(
                openapi,
                http::StatusCode::BAD_GATEWAY,
                "Upstream protocol failure",
            )
            .problem_response(
                openapi,
                http::StatusCode::SERVICE_UNAVAILABLE,
                "Upstream unavailable",
            )
            .problem_response(
                openapi,
                http::StatusCode::GATEWAY_TIMEOUT,
                "Upstream timed out",
            )
            .register(router, openapi);
    }
    router = proxy_unregistered_fallback(router, PROXY_ALIAS_PATH);
    proxy_unregistered_fallback(router, PROXY_SUFFIX_PATH)
}

/// Attach the fallback of one proxy path.
///
/// `TRACE`, `CONNECT` and extension methods reach the data plane through it and
/// are answered with the problem contract instead of axum's bare `405`.
fn proxy_unregistered_fallback(router: Router, path: &str) -> Router {
    router.route(
        path,
        axum::routing::MethodRouter::new().fallback(handlers::proxy_unregistered_method),
    )
}

/// `OpenAPI` response of a proxied request: whatever the upstream returned.
fn passthrough_response() -> ResponseSpec {
    ResponseSpec {
        status: http::StatusCode::OK.as_u16(),
        content_type: "*/*",
        description: "Upstream response, marked `X-OAGW-Error-Source: upstream`".to_owned(),
        schema: None,
    }
}

/// Method router for one proxied method.
///
/// Composed here because `OperationBuilder::handler` maps only the five schema
/// methods and the proxy must accept `HEAD` and `OPTIONS` as well. The router
/// carries **no** fallback: the one of the path is attached once, after every
/// method has been registered.
fn proxy_method_router(method: Method) -> axum::routing::MethodRouter<()> {
    match method {
        m if m == Method::GET => axum::routing::get(handlers::proxy_alias),
        m if m == Method::POST => axum::routing::post(handlers::proxy_alias),
        m if m == Method::PUT => axum::routing::put(handlers::proxy_alias),
        m if m == Method::DELETE => axum::routing::delete(handlers::proxy_alias),
        m if m == Method::PATCH => axum::routing::patch(handlers::proxy_alias),
        m if m == Method::HEAD => axum::routing::head(handlers::proxy_alias),
        _ => axum::routing::options(handlers::proxy_alias),
    }
}
