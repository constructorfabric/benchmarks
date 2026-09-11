//! REST route registration.
//!
//! Paths are **gear-relative**: the API gateway nests this router under its
//! own `prefix_path`, so registering `/oagw/v1/...` here is what makes
//! `<prefix>/oagw/v1/...` reachable. Repeating a prefix would double it.

use std::sync::Arc;

use axum::Router;
use http::{Method, StatusCode};
use toolkit::api::{OpenApiRegistry, OperationBuilder, ParamLocation, ParamSpec};

use crate::api::rest::dto::{PluginDto, PluginSourceDto, RouteDto, UpstreamDto};
use crate::api::rest::error::fill_problem_instance;
use crate::api::rest::handlers;
use crate::domain::services::management::ControlPlaneService;
use crate::infra::proxy::service::DataPlaneService;

const MANAGEMENT_TAG: &str = "OAGW Management";
const PROXY_TAG: &str = "OAGW Proxy";

/// Gear-relative base path of the management API.
pub const UPSTREAMS_PATH: &str = "/oagw/v1/upstreams";
/// Gear-relative base path of the route API.
pub const ROUTES_PATH: &str = "/oagw/v1/routes";
/// Gear-relative base path of the plugin API.
pub const PLUGINS_PATH: &str = "/oagw/v1/plugins";
/// Gear-relative proxy endpoint, without a path suffix.
pub const PROXY_PATH: &str = "/oagw/v1/proxy/{alias}";
/// Gear-relative proxy endpoint, with a path suffix.
pub const PROXY_PATH_SUFFIX: &str = "/oagw/v1/proxy/{alias}/{*path}";

/// Methods the proxy endpoint accepts.
///
/// `OPTIONS` is registered so a browser preflight reaches the handler, which
/// answers it without resolving an upstream (ADR-0004).
const PROXY_METHODS: &[Method] = &[
    Method::GET,
    Method::POST,
    Method::PUT,
    Method::PATCH,
    Method::DELETE,
    Method::HEAD,
    Method::OPTIONS,
];

/// The `OData` system query options every list endpoint binds.
macro_rules! odata_params {
    ($builder:expr) => {
        $builder
            .query_param(
                "$filter",
                false,
                "OData filter, e.g. alias eq 'api.openai.com'",
            )
            .query_param("$select", false, "Comma-separated fields to return")
            .query_param("$orderby", false, "Sort order, e.g. created_at desc")
            .query_param_typed(
                "$top",
                false,
                "Maximum results (default 50, max 100)",
                "integer",
            )
            .query_param_typed("$skip", false, "Offset for pagination", "integer")
    };
}

fn target_host_param() -> ParamSpec {
    ParamSpec {
        name: "X-OAGW-Target-Host".to_owned(),
        location: ParamLocation::Header,
        required: false,
        description: Some(
            "Selects one endpoint of a multi-endpoint upstream, bypassing round-robin. \
             Required when the alias was derived from a shared domain suffix, because such an \
             alias names no single endpoint. Consumed during routing and never forwarded."
                .to_owned(),
        ),
        param_type: "string".to_owned(),
        array: false,
    }
}

/// Register every OAGW REST route on `router`.
///
/// The gear's routes are built on a fresh router and merged in, so the
/// problem-details layer and the service extensions attach to OAGW's own
/// routes only — `router` arrives carrying every previously-registered gear's
/// routes, and layering onto it directly would put OAGW middleware in front
/// of them.
pub fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    control_plane: Arc<ControlPlaneService>,
    data_plane: Arc<DataPlaneService>,
) -> Router {
    let mine = register_upstreams(Router::new(), openapi);
    let mine = register_routes_api(mine, openapi);
    let mine = register_plugins(mine, openapi);
    let mine = register_proxy(mine, openapi);

    router.merge(
        mine.route_layer(axum::middleware::from_fn(fill_problem_instance))
            .layer(axum::Extension(control_plane))
            .layer(axum::Extension(data_plane)),
    )
}

fn register_upstreams(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::post(UPSTREAMS_PATH)
        .operation_id("oagw.create_upstream")
        .summary("Create an upstream")
        .description(
            "Register an external service. The alias is auto-derived for hostname endpoints \
             and required for IP-based or otherwise non-derivable endpoint pools.",
        )
        .tag(MANAGEMENT_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<UpstreamDto>(openapi, "Upstream configuration")
        .handler(handlers::create_upstream)
        .json_response_with_schema::<UpstreamDto>(openapi, StatusCode::CREATED, "Upstream created")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = odata_params!(
        OperationBuilder::get(UPSTREAMS_PATH)
            .operation_id("oagw.list_upstreams")
            .summary("List upstreams")
            .description("List the calling tenant's upstreams.")
            .tag(MANAGEMENT_TAG)
            .authenticated()
            .no_license_required()
    )
    .handler(handlers::list_upstreams)
    .json_response(StatusCode::OK, "Page of upstreams: {items, page_info}")
    .error_400(openapi)
    .error_401(openapi)
    .error_403(openapi)
    .error_500(openapi)
    .register(router, openapi);

    let router = OperationBuilder::get(format!("{UPSTREAMS_PATH}/{{id}}"))
        .operation_id("oagw.get_upstream")
        .summary("Get an upstream")
        .description("Retrieve one of the calling tenant's upstreams.")
        .tag(MANAGEMENT_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "`gts.cf.core.oagw.upstream.v1~<uuid>` or a bare UUID")
        .handler(handlers::get_upstream)
        .json_response_with_schema::<UpstreamDto>(openapi, StatusCode::OK, "Upstream")
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::put(format!("{UPSTREAMS_PATH}/{{id}}"))
        .operation_id("oagw.replace_upstream")
        .summary("Replace an upstream")
        .description(
            "Full replacement: omitted optional blocks are cleared. The alias is immutable, \
             so an endpoint change that would move it is rejected.",
        )
        .tag(MANAGEMENT_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "`gts.cf.core.oagw.upstream.v1~<uuid>` or a bare UUID")
        .json_request::<UpstreamDto>(openapi, "Replacement upstream configuration")
        .handler(handlers::replace_upstream)
        .json_response_with_schema::<UpstreamDto>(openapi, StatusCode::OK, "Upstream replaced")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    OperationBuilder::delete(format!("{UPSTREAMS_PATH}/{{id}}"))
        .operation_id("oagw.delete_upstream")
        .summary("Delete an upstream")
        .description("Delete an upstream and cascade to its routes.")
        .tag(MANAGEMENT_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "`gts.cf.core.oagw.upstream.v1~<uuid>` or a bare UUID")
        .handler(handlers::delete_upstream)
        .no_content_response(StatusCode::NO_CONTENT, "Upstream deleted")
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi)
}

fn register_routes_api(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::post(ROUTES_PATH)
        .operation_id("oagw.create_route")
        .summary("Create a route")
        .description(
            "Register an API path on one of the calling tenant's upstreams. An ancestor's \
             upstream is not directly addressable.",
        )
        .tag(MANAGEMENT_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<RouteDto>(openapi, "Route configuration")
        .handler(handlers::create_route)
        .json_response_with_schema::<RouteDto>(openapi, StatusCode::CREATED, "Route created")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = odata_params!(
        OperationBuilder::get(ROUTES_PATH)
            .operation_id("oagw.list_routes")
            .summary("List routes")
            .description("List the calling tenant's routes.")
            .tag(MANAGEMENT_TAG)
            .authenticated()
            .no_license_required()
    )
    .handler(handlers::list_routes)
    .json_response(StatusCode::OK, "Page of routes: {items, page_info}")
    .error_400(openapi)
    .error_401(openapi)
    .error_403(openapi)
    .error_500(openapi)
    .register(router, openapi);

    let router = OperationBuilder::get(format!("{ROUTES_PATH}/{{id}}"))
        .operation_id("oagw.get_route")
        .summary("Get a route")
        .description("Retrieve one of the calling tenant's routes.")
        .tag(MANAGEMENT_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "`gts.cf.core.oagw.route.v1~<uuid>` or a bare UUID")
        .handler(handlers::get_route)
        .json_response_with_schema::<RouteDto>(openapi, StatusCode::OK, "Route")
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::put(format!("{ROUTES_PATH}/{{id}}"))
        .operation_id("oagw.replace_route")
        .summary("Replace a route")
        .description("Full replacement. `upstream_id` is immutable.")
        .tag(MANAGEMENT_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "`gts.cf.core.oagw.route.v1~<uuid>` or a bare UUID")
        .json_request::<RouteDto>(openapi, "Replacement route configuration")
        .handler(handlers::replace_route)
        .json_response_with_schema::<RouteDto>(openapi, StatusCode::OK, "Route replaced")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    OperationBuilder::delete(format!("{ROUTES_PATH}/{{id}}"))
        .operation_id("oagw.delete_route")
        .summary("Delete a route")
        .description("Delete one of the calling tenant's routes.")
        .tag(MANAGEMENT_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "`gts.cf.core.oagw.route.v1~<uuid>` or a bare UUID")
        .handler(handlers::delete_route)
        .no_content_response(StatusCode::NO_CONTENT, "Route deleted")
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi)
}

fn register_plugins(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::post(PLUGINS_PATH)
        .operation_id("oagw.create_plugin")
        .summary("Create a custom plugin")
        .description(
            "Register a tenant-defined plugin. Plugins are immutable after creation: publish a \
             change as a new plugin and re-bind the references.",
        )
        .tag(MANAGEMENT_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<PluginDto>(openapi, "Plugin definition, including its source")
        .handler(handlers::create_plugin)
        .json_response_with_schema::<PluginDto>(openapi, StatusCode::CREATED, "Plugin created")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = odata_params!(
        OperationBuilder::get(PLUGINS_PATH)
            .operation_id("oagw.list_plugins")
            .summary("List custom plugins")
            .description("List the calling tenant's custom plugins.")
            .tag(MANAGEMENT_TAG)
            .authenticated()
            .no_license_required()
    )
    .handler(handlers::list_plugins)
    .json_response(StatusCode::OK, "Page of plugins: {items, page_info}")
    .error_400(openapi)
    .error_401(openapi)
    .error_403(openapi)
    .error_500(openapi)
    .register(router, openapi);

    let router = OperationBuilder::get(format!("{PLUGINS_PATH}/{{id}}"))
        .operation_id("oagw.get_plugin")
        .summary("Get a custom plugin")
        .description("Retrieve a plugin definition. The source is served separately.")
        .tag(MANAGEMENT_TAG)
        .authenticated()
        .no_license_required()
        .path_param(
            "id",
            "`gts.cf.core.oagw.{type}_plugin.v1~<uuid>` or a bare UUID",
        )
        .handler(handlers::get_plugin)
        .json_response_with_schema::<PluginDto>(openapi, StatusCode::OK, "Plugin")
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(format!("{PLUGINS_PATH}/{{id}}/source"))
        .operation_id("oagw.get_plugin_source")
        .summary("Get a plugin's source")
        .description("Retrieve the plugin's script source.")
        .tag(MANAGEMENT_TAG)
        .authenticated()
        .no_license_required()
        .path_param(
            "id",
            "`gts.cf.core.oagw.{type}_plugin.v1~<uuid>` or a bare UUID",
        )
        .handler(handlers::get_plugin_source)
        .json_response_with_schema::<PluginSourceDto>(openapi, StatusCode::OK, "Plugin source")
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    OperationBuilder::delete(format!("{PLUGINS_PATH}/{{id}}"))
        .operation_id("oagw.delete_plugin")
        .summary("Delete a custom plugin")
        .description("Delete an unlinked plugin. A referenced plugin answers 409.")
        .tag(MANAGEMENT_TAG)
        .authenticated()
        .no_license_required()
        .path_param(
            "id",
            "`gts.cf.core.oagw.{type}_plugin.v1~<uuid>` or a bare UUID",
        )
        .handler(handlers::delete_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "Plugin deleted")
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi)
}

fn register_proxy(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    for path in [PROXY_PATH, PROXY_PATH_SUFFIX] {
        let with_suffix = path == PROXY_PATH_SUFFIX;
        for method in PROXY_METHODS {
            let mut builder = OperationBuilder::new(method.clone(), path)
                .operation_id(format!(
                    "oagw.proxy_{}{}",
                    method.as_str().to_ascii_lowercase(),
                    if with_suffix { "_path" } else { "" }
                ))
                .summary("Proxy a request to an upstream")
                .description(
                    "Resolve the alias across the tenant hierarchy, match a route, inject \
                     credentials, apply the plugin chain and forward. Plain HTTP responses, \
                     server-sent-event streams and protocol upgrades are all passed through.",
                )
                .tag(PROXY_TAG)
                .path_param("alias", "Upstream routing alias")
                .param(target_host_param());
            if with_suffix {
                builder = builder.path_param("path", "Path suffix appended to the route path");
            }
            router = builder
                .authenticated()
                .no_license_required()
                .method_router(
                    axum::routing::MethodRouter::<()>::new()
                        .on(method_filter(method), handlers::proxy),
                )
                .json_response(StatusCode::OK, "Upstream response, passed through")
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
    }
    router
}

fn method_filter(method: &Method) -> axum::routing::MethodFilter {
    use axum::routing::MethodFilter;
    match *method {
        Method::POST => MethodFilter::POST,
        Method::PUT => MethodFilter::PUT,
        Method::PATCH => MethodFilter::PATCH,
        Method::DELETE => MethodFilter::DELETE,
        Method::HEAD => MethodFilter::HEAD,
        Method::OPTIONS => MethodFilter::OPTIONS,
        _ => MethodFilter::GET,
    }
}

#[cfg(test)]
#[path = "routes_tests.rs"]
mod tests;
