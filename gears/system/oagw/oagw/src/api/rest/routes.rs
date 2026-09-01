//! REST route registration for the OAGW gear.
//!
//! Paths are gear-relative: `DESIGN` §3.3 writes `/api/oagw/v1/...`, where the
//! `/api` segment is the operator's gateway `prefix_path` and is therefore
//! never written here.

use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use toolkit::api::{OpenApiRegistry, OperationBuilder, OperationSpec, ParamLocation, ParamSpec};

use super::dto::{
    ListEnvelopeDto, PluginRequestDto, PluginResponseDto, PluginSourceResponseDto, RouteRequestDto,
    RouteResponseDto, UpstreamRequestDto, UpstreamResponseDto,
};
use super::handlers;
use crate::domain::services::ControlPlaneService;
use crate::infra::proxy::forward::ProxyEngine;

const TAG: &str = "Outbound API Gateway";

/// Register all REST routes of the gear.
pub fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    service: Arc<ControlPlaneService>,
    engine: Arc<ProxyEngine>,
) -> Router {
    let router = upstream_routes(router, openapi);
    let router = route_routes(router, openapi);
    let router = plugin_routes(router, openapi);
    let router = proxy_routes(router, openapi);
    let router = router.layer(axum::Extension(engine));
    router.layer(axum::Extension(service))
}

/// `{id}` path parameter of a management resource.
fn id_param() -> ParamSpec {
    ParamSpec {
        name: "id".to_owned(),
        location: ParamLocation::Path,
        required: true,
        description: Some(
            "Anonymous GTS resource id: `gts.cf.core.oagw.<type>.v1~<uuid>`".to_owned(),
        ),
        param_type: "string".to_owned(),
        array: false,
    }
}

/// `{alias}` path parameter of the proxy API.
fn alias_param() -> ParamSpec {
    ParamSpec {
        name: "alias".to_owned(),
        location: ParamLocation::Path,
        required: true,
        description: Some("Routing key of the upstream to proxy to".to_owned()),
        param_type: "string".to_owned(),
        array: false,
    }
}

/// One of the five `OData` system query options the lists accept.
fn odata_param(name: &'static str, description: &'static str) -> ParamSpec {
    ParamSpec {
        name: name.to_owned(),
        location: ParamLocation::Query,
        required: false,
        description: Some(description.to_owned()),
        param_type: "string".to_owned(),
        array: false,
    }
}

fn upstream_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::post("/oagw/v1/upstreams")
        .operation_id("oagw.create_upstream")
        .summary("Create an upstream")
        .description(
            "Create a tenant-scoped upstream. The alias is derived from the endpoint pool \
             unless the pool cannot derive one, in which case it is required.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<UpstreamRequestDto>(openapi, "Upstream payload")
        .handler(handlers::create_upstream)
        .json_response_with_schema::<UpstreamResponseDto>(
            openapi,
            StatusCode::CREATED,
            "The created upstream",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/upstreams")
        .operation_id("oagw.list_upstreams")
        .summary("List upstreams")
        .description(
            "List the upstreams of the calling tenant. Supports `$filter`, `$select`, \
             `$orderby`, `$top` (default 50, max 100) and `$skip`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .param(odata_param(
            "$filter",
            "OData filter expression, e.g. `alias eq 'api.openai.com'`",
        ))
        .param(odata_param("$select", "Comma-separated fields to return"))
        .param(odata_param(
            "$orderby",
            "Sort order, e.g. `created_at desc`",
        ))
        .param(odata_param("$top", "Max results (default 50, max 100)"))
        .param(odata_param("$skip", "Offset pagination"))
        .handler(handlers::list_upstreams)
        .json_response_with_schema::<ListEnvelopeDto>(
            openapi,
            StatusCode::OK,
            "A page of upstreams",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.get_upstream")
        .summary("Get an upstream")
        .description("Read one upstream owned by the calling tenant.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .param(id_param())
        .handler(handlers::get_upstream)
        .json_response_with_schema::<UpstreamResponseDto>(
            openapi,
            StatusCode::OK,
            "The requested upstream",
        )
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
            "Full replacement. The alias is immutable: a hostname-derived alias may only be \
             recomputed to the same value, and a derivable pool may not become non-derivable.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .param(id_param())
        .json_request::<UpstreamRequestDto>(openapi, "Upstream payload")
        .handler(handlers::replace_upstream)
        .json_response_with_schema::<UpstreamResponseDto>(
            openapi,
            StatusCode::OK,
            "The replaced upstream",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    OperationBuilder::delete("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.delete_upstream")
        .summary("Delete an upstream")
        .description("Delete an upstream owned by the calling tenant, and its routes.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .param(id_param())
        .handler(handlers::delete_upstream)
        .no_content_response(StatusCode::NO_CONTENT, "Upstream deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi)
}

fn route_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::post("/oagw/v1/routes")
        .operation_id("oagw.create_route")
        .summary("Create a route")
        .description(
            "Create a route on an upstream of the calling tenant. The match rule must be \
             unique within the upstream at its priority.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<RouteRequestDto>(openapi, "Route payload")
        .handler(handlers::create_route)
        .json_response_with_schema::<RouteResponseDto>(
            openapi,
            StatusCode::CREATED,
            "The created route",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/routes")
        .operation_id("oagw.list_routes")
        .summary("List routes")
        .description(
            "List the routes of the calling tenant. Supports `$filter`, `$select`, \
             `$orderby`, `$top` and `$skip`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .param(odata_param(
            "$filter",
            "OData filter expression, e.g. `alias eq 'api.openai.com'`",
        ))
        .param(odata_param("$select", "Comma-separated fields to return"))
        .param(odata_param(
            "$orderby",
            "Sort order, e.g. `created_at desc`",
        ))
        .param(odata_param("$top", "Max results (default 50, max 100)"))
        .param(odata_param("$skip", "Offset pagination"))
        .handler(handlers::list_routes)
        .json_response_with_schema::<ListEnvelopeDto>(openapi, StatusCode::OK, "A page of routes")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/routes/{id}")
        .operation_id("oagw.get_route")
        .summary("Get a route")
        .description("Read one route owned by the calling tenant.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .param(id_param())
        .handler(handlers::get_route)
        .json_response_with_schema::<RouteResponseDto>(
            openapi,
            StatusCode::OK,
            "The requested route",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::put("/oagw/v1/routes/{id}")
        .operation_id("oagw.replace_route")
        .summary("Replace a route")
        .description(
            "Full replacement. `upstream_id` is immutable and must repeat the current value.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .param(id_param())
        .json_request::<RouteRequestDto>(openapi, "Route payload")
        .handler(handlers::replace_route)
        .json_response_with_schema::<RouteResponseDto>(
            openapi,
            StatusCode::OK,
            "The replaced route",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    OperationBuilder::delete("/oagw/v1/routes/{id}")
        .operation_id("oagw.delete_route")
        .summary("Delete a route")
        .description("Delete a route owned by the calling tenant.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .param(id_param())
        .handler(handlers::delete_route)
        .no_content_response(StatusCode::NO_CONTENT, "Route deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi)
}

fn plugin_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.create_plugin")
        .summary("Create a custom plugin")
        .description("Store a custom Starlark plugin of the calling tenant.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<PluginRequestDto>(openapi, "Plugin payload")
        .handler(handlers::create_plugin)
        .json_response_with_schema::<PluginResponseDto>(
            openapi,
            StatusCode::CREATED,
            "The created plugin",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/plugins")
        .operation_id("oagw.list_plugins")
        .summary("List plugins")
        .description(
            "List the custom plugins of the calling tenant. Supports `$filter`, `$select`, \
             `$top` and `$skip`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .param(odata_param(
            "$filter",
            "OData filter expression, e.g. `type eq 'guard'`",
        ))
        .param(odata_param("$select", "Comma-separated fields to return"))
        .param(odata_param(
            "$orderby",
            "Sort order, e.g. `created_at desc`",
        ))
        .param(odata_param("$top", "Max results (default 50, max 100)"))
        .param(odata_param("$skip", "Offset pagination"))
        .handler(handlers::list_plugins)
        .json_response_with_schema::<ListEnvelopeDto>(openapi, StatusCode::OK, "A page of plugins")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/plugins/{id}")
        .operation_id("oagw.get_plugin")
        .summary("Get a plugin")
        .description("Read one custom plugin owned by the calling tenant.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .param(id_param())
        .handler(handlers::get_plugin)
        .json_response_with_schema::<PluginResponseDto>(
            openapi,
            StatusCode::OK,
            "The requested plugin",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/plugins/{id}/source")
        .operation_id("oagw.get_plugin_source")
        .summary("Get a plugin's Starlark source")
        .description("Return the Starlark source of a custom plugin, as `text/x-starlark`.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .param(id_param())
        .handler(handlers::get_plugin_source)
        .json_response_with_schema::<PluginSourceResponseDto>(
            openapi,
            StatusCode::OK,
            "The Starlark source",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    OperationBuilder::delete("/oagw/v1/plugins/{id}")
        .operation_id("oagw.delete_plugin")
        .summary("Delete a plugin")
        .description(
            "Delete an unreferenced custom plugin. Returns `409 PluginInUse` with \
             `referenced_by` when an upstream or a route still binds it.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .param(id_param())
        .handler(handlers::delete_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "Plugin deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi)
}

/// The `200` a proxied call answers with: whatever status the upstream
/// produced, with its body streamed (`DESIGN` §3.5).
fn proxied_response() -> toolkit::api::ResponseSpec {
    toolkit::api::ResponseSpec {
        status: 200,
        content_type: "*/*",
        description: "The upstream response, streamed, carrying X-OAGW-Error-Source: upstream."
            .to_owned(),
        schema: None,
    }
}

/// The data-plane methods the `OpenAPI` renderer can spell.
///
/// `toolkit::api::openapi_registry` maps an operation onto one of
/// `Get`/`Post`/`Put`/`Delete`/`Patch` and falls back to `Get` for any other
/// method, so `HEAD` and `OPTIONS` have no distinct document entry. They stay
/// mounted — the route is registered with `routing::any` — and the `GET`
/// operation documents their request and response shape.
const DECLARED_METHODS: &[axum::http::Method] = &[
    axum::http::Method::GET,
    axum::http::Method::POST,
    axum::http::Method::PUT,
    axum::http::Method::DELETE,
    axum::http::Method::PATCH,
];

/// Register the data plane of one proxy path.
///
/// The route is mounted once, with `routing::any`, because axum refuses two
/// registrations for the same path; the `OpenAPI` contract is then declared once
/// per accepted method so the document matches the mounted method set. Every
/// operation delegates to the same handler, so each spec is the base one with a
/// different method and operation id.
fn declare_proxy_operations(base: &OperationSpec, id_prefix: &str, openapi: &dyn OpenApiRegistry) {
    let mounted = base.method.clone();
    for method in DECLARED_METHODS {
        // The mounted spec is already registered by `.register(...)`.
        if *method == mounted {
            continue;
        }
        let mut spec = base.clone();
        spec.method = method.clone();
        spec.operation_id = Some(format!(
            "{id_prefix}.{}",
            method.as_str().to_ascii_lowercase()
        ));
        openapi.register_operation(&spec);
    }
}

fn proxy_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    // Two paths, because axum's `{*suffix}` wildcard does not match the
    // alias-only form. Each is mounted once with `routing::any`: the accepted
    // method set is a property of the matched route, not of the gateway path.
    let invoke = OperationBuilder::new(axum::http::Method::GET, "/oagw/v1/proxy/{alias}")
        .operation_id("oagw.proxy.invoke.get")
        .summary("Proxy a request to an upstream")
        .description(
            "Data-plane entry point for the alias routing key. Accepts every HTTP method, \
             streams the upstream answer back and tunnels a WebSocket upgrade. `429`, \
             `502`, `503` and `504` are gateway problems carrying \
             `X-OAGW-Error-Source: gateway`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .param(alias_param())
        .method_router(axum::routing::any(handlers::proxy))
        .response(proxied_response())
        .error_503(openapi);
    declare_proxy_operations(invoke.spec(), "oagw.proxy.invoke", openapi);
    let router = invoke.register(router, openapi);

    let with_suffix = OperationBuilder::new(
        axum::http::Method::GET,
        "/oagw/v1/proxy/{alias}/{*path_suffix}",
    )
    .operation_id("oagw.proxy.invoke_with_suffix.get")
    .summary("Proxy a request with a path suffix")
    .description(
        "Data-plane entry point carrying a path suffix that the matched route may \
                 append to the upstream path. Accepts every HTTP method, streams the \
                 upstream answer back and tunnels a WebSocket upgrade.",
    )
    .tag(TAG)
    .authenticated()
    .no_license_required()
    .param(alias_param())
    .method_router(axum::routing::any(handlers::proxy_with_suffix))
    .response(proxied_response())
    .error_503(openapi);
    declare_proxy_operations(with_suffix.spec(), "oagw.proxy.invoke_with_suffix", openapi);
    with_suffix.register(router, openapi)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "routes_tests.rs"]
mod tests;
