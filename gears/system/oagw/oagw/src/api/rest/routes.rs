//! Route registrations of the OAGW management API.
//!
//! Fifteen operations under `/oagw/v1`, registered through
//! [`OperationBuilder`] so the OpenAPI document is generated with the
//! platform's canonical `Problem` schema, the `authenticated` security
//! requirement and the `OAGW` tag.
//!
//! ## List envelope (documented choice)
//!
//! List endpoints return the platform's `toolkit::Page<T>` envelope
//! (`{ items, page_info }`) rather than a bare array, so the OAGW lists stay
//! wire-compatible with every other gear and with the platform SDK paging
//! helpers. Offset paging needs no cursor, so `page_info.next_cursor` and
//! `prev_cursor` stay `null` and `limit` carries the effective `$top`.
//!
//! Because `$select` projects fields out of the serialised items, the runtime
//! body carries the *projected* objects while the OpenAPI document declares
//! the full item schema (`UpstreamDto`/`RouteDto`/`PluginDto`) inside
//! `Page<…>`: utoipa cannot express a `$select`-dependent shape.
//!
//! ## No plugin `PUT`
//!
//! Plugins are immutable (DESIGN section 3.3), so there is no
//! `PUT /oagw/v1/plugins/{id}`: a re-registration of the same id is a `409`
//! and a redefinition must be modelled as a new plugin.

use axum::Router;
use http::StatusCode;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationBuilder;

use crate::api::rest::dto::{
    CreatePluginRequest, CreateRouteRequest, PluginDto, PluginPageDto, PluginSourceDto,
    ReplaceRouteRequest, RouteDto, RoutePageDto, UpstreamDto, UpstreamPageDto, UpstreamRequest,
};
use crate::api::rest::handlers;

/// Tag of the management operations in the OpenAPI document.
const API_TAG: &str = "OAGW Management";

/// Registers the management routes of the gear.
#[allow(clippy::needless_pass_by_value)]
pub fn register_routes(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    // -- upstreams -----------------------------------------------------------

    router = OperationBuilder::get("/oagw/v1/upstreams")
        .operation_id("oagw.list_upstreams")
        .summary("List upstreams")
        .description("List the upstreams of the calling tenant, newest first.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::upstreams::list_upstreams)
        .json_response_with_schema::<UpstreamPageDto>(
            openapi,
            StatusCode::OK,
            "Paginated upstreams",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/oagw/v1/upstreams")
        .operation_id("oagw.create_upstream")
        .summary("Create an upstream")
        .description(
            "Create an upstream for the calling tenant. The alias is derived from the endpoint \
             pool when every endpoint is a hostname; a supplied alias must match it.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<UpstreamRequest>(openapi, "Upstream creation draft")
        .handler(handlers::upstreams::create_upstream)
        .json_response_with_schema::<UpstreamDto>(openapi, StatusCode::CREATED, "Created upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.get_upstream")
        .summary("Read an upstream")
        .description("Read one upstream of the calling tenant.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream instance id")
        .handler(handlers::upstreams::get_upstream)
        .json_response_with_schema::<UpstreamDto>(openapi, StatusCode::OK, "The upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.replace_upstream")
        .summary("Replace an upstream")
        .description(
            "Replace an upstream of the calling tenant. The alias and the endpoint pool derived \
             from it are immutable; omitted optional sections are cleared.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream instance id")
        .json_request::<UpstreamRequest>(openapi, "Upstream replacement draft")
        .handler(handlers::upstreams::replace_upstream)
        .json_response_with_schema::<UpstreamDto>(openapi, StatusCode::OK, "Replaced upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.delete_upstream")
        .summary("Delete an upstream")
        .description("Delete an upstream of the calling tenant together with its routes.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream instance id")
        .handler(handlers::upstreams::delete_upstream)
        .no_content_response(StatusCode::NO_CONTENT, "Upstream deleted")
        .standard_errors(openapi)
        .register(router, openapi);

    // -- routes --------------------------------------------------------------

    router = OperationBuilder::get("/oagw/v1/routes")
        .operation_id("oagw.list_routes")
        .summary("List routes")
        .description("List the routes of the calling tenant, highest priority first.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::routes::list_routes)
        .json_response_with_schema::<RoutePageDto>(openapi, StatusCode::OK, "Paginated routes")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/oagw/v1/routes")
        .operation_id("oagw.create_route")
        .summary("Create a route")
        .description("Create a route under an upstream of the calling tenant.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<CreateRouteRequest>(openapi, "Route creation draft")
        .handler(handlers::routes::create_route)
        .json_response_with_schema::<RouteDto>(openapi, StatusCode::CREATED, "Created route")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes/{id}")
        .operation_id("oagw.get_route")
        .summary("Read a route")
        .description("Read one route of the calling tenant.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route instance id")
        .handler(handlers::routes::get_route)
        .json_response_with_schema::<RouteDto>(openapi, StatusCode::OK, "The route")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/routes/{id}")
        .operation_id("oagw.replace_route")
        .summary("Replace a route")
        .description(
            "Replace a route of the calling tenant. `upstream_id` is immutable and the stored \
             value is kept.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route instance id")
        .json_request::<ReplaceRouteRequest>(openapi, "Route replacement draft")
        .handler(handlers::routes::replace_route)
        .json_response_with_schema::<RouteDto>(openapi, StatusCode::OK, "Replaced route")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/routes/{id}")
        .operation_id("oagw.delete_route")
        .summary("Delete a route")
        .description("Delete a route of the calling tenant.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route instance id")
        .handler(handlers::routes::delete_route)
        .no_content_response(StatusCode::NO_CONTENT, "Route deleted")
        .standard_errors(openapi)
        .register(router, openapi);

    // -- plugins -------------------------------------------------------------

    router = OperationBuilder::get("/oagw/v1/plugins")
        .operation_id("oagw.list_plugins")
        .summary("List plugins")
        .description("List the plugins of the calling tenant.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::plugins::list_plugins)
        .json_response_with_schema::<PluginPageDto>(openapi, StatusCode::OK, "Paginated plugins")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.create_plugin")
        .summary("Register a plugin")
        .description(
            "Register a plugin for the calling tenant. Plugins are immutable: there is no PUT.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<CreatePluginRequest>(openapi, "Plugin registration draft")
        .handler(handlers::plugins::create_plugin)
        .json_response_with_schema::<PluginDto>(openapi, StatusCode::CREATED, "Registered plugin")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{id}")
        .operation_id("oagw.get_plugin")
        .summary("Read a plugin")
        .description("Read one plugin of the calling tenant.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin instance id")
        .handler(handlers::plugins::get_plugin)
        .json_response_with_schema::<PluginDto>(openapi, StatusCode::OK, "The plugin")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{id}/source")
        .operation_id("oagw.get_plugin_source")
        .summary("Read the plugin definition source")
        .description(
            "Return the deterministic rendering of the plugin definition, for inspection and \
             review.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin instance id")
        .handler(handlers::plugins::get_plugin_source)
        .json_response_with_schema::<PluginSourceDto>(
            openapi,
            StatusCode::OK,
            "Rendered plugin source",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/plugins/{id}")
        .operation_id("oagw.delete_plugin")
        .summary("Delete a plugin")
        .description(
            "Delete a plugin of the calling tenant. The request is rejected with `409` while an \
             upstream or a route still references the plugin.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin instance id")
        .handler(handlers::plugins::delete_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "Plugin deleted")
        .standard_errors(openapi)
        .register(router, openapi);

    router
}

/// Tag of the data-plane operations in the OpenAPI document.
const PROXY_TAG: &str = "OAGW Proxy";

/// Content type of the proxied response in the OpenAPI document.
const PROXY_MEDIA_TYPE: &str = "*/*";

/// Content type of the metrics endpoint in the OpenAPI document.
const METRICS_MEDIA_TYPE: &str = "text/plain; version=0.0.4";

/// Registers the data-plane routes: the two proxy operations and the metrics
/// endpoint.
///
/// The proxy surface answers *every* HTTP method, so the routes are attached
/// with [`axum::routing::any`] rather than one `OperationBuilder` per verb;
/// the OpenAPI document declares each operation once, with the generic
/// `*/*` response the upstream actually produces.
pub fn register_proxy_routes(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    router = OperationBuilder::get("/oagw/v1/proxy/{alias}")
        .operation_id("oagw.proxy")
        .summary("Proxy an HTTP request")
        .description(
            "Forward a request to the upstream that owns the alias, after route resolution, \
             CORS, rate limiting and the plugin chain.",
        )
        .tag(PROXY_TAG)
        .authenticated()
        .no_license_required()
        .method_router(axum::routing::any(handlers::proxy::proxy_alias))
        .text_response(
            http::StatusCode::OK,
            "Proxied upstream response",
            PROXY_MEDIA_TYPE,
        )
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/proxy/{alias}/{*path_suffix}")
        .operation_id("oagw.proxy_path")
        .summary("Proxy an HTTP request with a path suffix")
        .description(
            "Forward a request whose path continues past the alias. The suffix is appended to \
             the matched route path unless the route disables suffix handling.",
        )
        .tag(PROXY_TAG)
        .authenticated()
        .no_license_required()
        .path_param("alias", "Upstream routing alias")
        .path_param("path_suffix", "Remainder of the request path")
        .method_router(axum::routing::any(handlers::proxy::proxy_alias_path))
        .text_response(
            http::StatusCode::OK,
            "Proxied upstream response",
            PROXY_MEDIA_TYPE,
        )
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/metrics")
        .operation_id("oagw.get_metrics")
        .summary("Read the data-plane metrics")
        .description(
            "Render the in-process metrics registry in the Prometheus text exposition format.",
        )
        .tag(PROXY_TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::proxy::get_metrics)
        .text_response(
            http::StatusCode::OK,
            "Metrics in Prometheus text format",
            METRICS_MEDIA_TYPE,
        )
        .register(router, openapi);

    router
}
