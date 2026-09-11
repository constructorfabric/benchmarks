// Created: 2026-09-01 by Constructor Tech
//! Route registration.
//!
//! `docs/DESIGN.md` §3.3. Every path is rooted at `/oagw/v1`; the management
//! API shares the router with the proxy, which is mounted with `any` on
//! `/proxy/{alias}` and `/proxy/{alias}/*suffix` so the suffix the route
//! matcher sees is the caller's own path verbatim.

use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationBuilder;

use crate::api::handlers;
use crate::api::proxy;

/// Tag under which the whole catalogue is documented.
const TAG: &str = "OAGW";

/// Register every `oagw` route onto `router`.
pub fn register(router: axum::Router, openapi: &dyn OpenApiRegistry) -> axum::Router {
    let router = upstreams(router, openapi);
    let router = routes(router, openapi);
    let router = plugins(router, openapi);
    proxy_routes(router, openapi)
}

fn upstreams(mut router: axum::Router, openapi: &dyn OpenApiRegistry) -> axum::Router {
    router = OperationBuilder::get("/oagw/v1/upstreams")
        .operation_id("oagw.list_upstreams")
        .summary("List upstreams")
        .description("List the calling tenant's own upstreams, optionally filtered by alias.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .query_param(
            "$filter",
            false,
            "OData-style filter, e.g. `alias eq 'api.openai.com'`",
        )
        .handler(handlers::list_upstreams)
        .json_response(http::StatusCode::OK, "The tenant's upstreams")
        .error_400(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/oagw/v1/upstreams")
        .operation_id("oagw.create_upstream")
        .summary("Create an upstream")
        .description("Create an upstream. The alias is derived from the endpoint pool unless it is one that cannot be derived from.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<crate::api::dto::UpstreamDto>(openapi, "The upstream to create")
        .handler(handlers::create_upstream)
        .json_response(http::StatusCode::CREATED, "The created upstream")
        .error_400(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.get_upstream")
        .summary("Read an upstream")
        .description(
            "Read one upstream by id or alias. An ancestor's upstream is not visible here.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream id or alias")
        .handler(handlers::get_upstream)
        .json_response(http::StatusCode::OK, "The upstream")
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.update_upstream")
        .summary("Replace an upstream")
        .description("Replace an upstream in full. The alias is immutable once set.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream id or alias")
        .json_request::<crate::api::dto::UpstreamDto>(openapi, "The replacement upstream")
        .handler(handlers::update_upstream)
        .json_response(http::StatusCode::OK, "The replaced upstream")
        .error_400(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    OperationBuilder::delete("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.delete_upstream")
        .summary("Delete an upstream")
        .description("Delete an upstream and every route that targets it.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream id or alias")
        .handler(handlers::delete_upstream)
        .no_content_response(http::StatusCode::NO_CONTENT, "The upstream is gone")
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi)
}

fn routes(mut router: axum::Router, openapi: &dyn OpenApiRegistry) -> axum::Router {
    router = OperationBuilder::get("/oagw/v1/routes")
        .operation_id("oagw.list_routes")
        .summary("List routes")
        .description("List the calling tenant's own routes.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::list_routes)
        .json_response(http::StatusCode::OK, "The tenant's routes")
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/oagw/v1/routes")
        .operation_id("oagw.create_route")
        .summary("Create a route")
        .description("Create a route binding a match rule to one of this tenant's upstreams.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<crate::api::dto::RouteDto>(openapi, "The route to create")
        .handler(handlers::create_route)
        .json_response(http::StatusCode::CREATED, "The created route")
        .error_400(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes/{id}")
        .operation_id("oagw.get_route")
        .summary("Read a route")
        .description("Read one route by id.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route id")
        .handler(handlers::get_route)
        .json_response(http::StatusCode::OK, "The route")
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/routes/{id}")
        .operation_id("oagw.update_route")
        .summary("Replace a route")
        .description("Replace a route in full. Its upstream binding is immutable.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route id")
        .json_request::<crate::api::dto::RouteDto>(openapi, "The replacement route")
        .handler(handlers::update_route)
        .json_response(http::StatusCode::OK, "The replaced route")
        .error_400(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    OperationBuilder::delete("/oagw/v1/routes/{id}")
        .operation_id("oagw.delete_route")
        .summary("Delete a route")
        .description("Delete one route by id.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route id")
        .handler(handlers::delete_route)
        .no_content_response(http::StatusCode::NO_CONTENT, "The route is gone")
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi)
}

fn plugins(mut router: axum::Router, openapi: &dyn OpenApiRegistry) -> axum::Router {
    router = OperationBuilder::get("/oagw/v1/plugins")
        .operation_id("oagw.list_plugins")
        .summary("List custom plugins")
        .description("List the calling tenant's own custom plugins.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::list_plugins)
        .json_response(http::StatusCode::OK, "The tenant's plugins")
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.create_plugin")
        .summary("Create a custom plugin")
        .description("Create a custom plugin definition, optionally scripted in Starlark.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<crate::api::dto::PluginDto>(openapi, "The plugin to create")
        .handler(handlers::create_plugin)
        .json_response(http::StatusCode::CREATED, "The created plugin")
        .error_400(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{id}")
        .operation_id("oagw.get_plugin")
        .summary("Read a custom plugin")
        .description("Read one custom plugin by id.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin id")
        .handler(handlers::get_plugin)
        .json_response(http::StatusCode::OK, "The plugin")
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{id}/source")
        .operation_id("oagw.plugin_source")
        .summary("Where a plugin is referenced")
        .description("List the upstreams and routes that bind a plugin, and its source.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin id")
        .handler(handlers::plugin_source)
        .json_response(http::StatusCode::OK, "The plugin's references")
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    OperationBuilder::delete("/oagw/v1/plugins/{id}")
        .operation_id("oagw.delete_plugin")
        .summary("Delete a custom plugin")
        .description("Delete a custom plugin, refusing with 409 while anything still binds it.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin id")
        .handler(handlers::delete_plugin)
        .no_content_response(http::StatusCode::NO_CONTENT, "The plugin is gone")
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi)
}

/// The proxy itself.
///
/// Registered as a plain axum route rather than an `OperationBuilder`
/// operation: the request is forwarded as it arrived, so the handler takes
/// the whole `Request` rather than typed extractors, and an `OPTIONS` on a
/// proxy path is answered by the CORS preflight rather than the method
/// router's own `405`.
fn proxy_routes(router: axum::Router, openapi: &dyn OpenApiRegistry) -> axum::Router {
    let _ = openapi;
    router
        .route("/oagw/v1/proxy/{alias}", axum::routing::any(proxy::proxy))
        .route("/oagw/v1/proxy/{alias}/", axum::routing::any(proxy::proxy))
        .route(
            "/oagw/v1/proxy/{alias}/{*suffix}",
            axum::routing::any(proxy::proxy),
        )
}
