// @cpt-begin:cpt-cf-oagw-dod-gear-foundation-router-mount:p1:inst-router
//! Route registration.
//!
//! Paths are gear-relative under `/oagw/v1`. The api-gateway gear nests its own
//! single global prefix over the whole assembled router, so this gear must not
//! repeat that prefix itself.

use super::dto::{
    PluginCreateDto, PluginDto, PluginListDto, RouteCreateDto, RouteDto, RouteListDto,
    RouteReplaceDto, UpstreamDto, UpstreamListDto, UpstreamWriteDto,
};
use super::handlers::{plugins, proxy, routes as route_handlers, upstreams};
use super::state::OagwState;
use axum::routing::options;
use axum::{Extension, Router};
use http::StatusCode;
use std::sync::Arc;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::{
    CORE_GLOBAL_BASE_LICENSE_FEATURE, LicenseFeature, OperationBuilder,
};

const MANAGEMENT_TAG: &str = "OAGW Management";
const PROXY_TAG: &str = "OAGW Proxy";

/// Base path for every route this gear registers.
pub const BASE_PATH: &str = "/oagw/v1";

struct License;

impl AsRef<str> for License {
    fn as_ref(&self) -> &'static str {
        CORE_GLOBAL_BASE_LICENSE_FEATURE
    }
}

impl LicenseFeature for License {}

/// Register every route the gear serves.
pub fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    state: Arc<OagwState>,
) -> Router {
    let router = register_upstreams(router, openapi);
    let router = register_routes_api(router, openapi);
    let router = register_plugins(router, openapi);
    let router = register_proxy(router, openapi);
    router.layer(Extension(state))
}

fn register_upstreams(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    router = OperationBuilder::post("/oagw/v1/upstreams")
        .operation_id("oagw.upstreams.create")
        .summary("Create an upstream")
        .description("Register an external service the gateway may proxy to.")
        .tag(MANAGEMENT_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .json_request::<UpstreamWriteDto>(openapi, "Upstream definition")
        .handler(upstreams::create)
        .json_response_with_schema::<UpstreamDto>(openapi, StatusCode::CREATED, "Created upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams")
        .operation_id("oagw.upstreams.list")
        .summary("List upstreams")
        .description("List the calling tenant's upstreams.")
        .tag(MANAGEMENT_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .query_param(
            "$top",
            false,
            "Maximum results, default 50 and capped at 100",
        )
        .query_param("$skip", false, "Offset into the result set")
        .query_param("$filter", false, "OData filter expression")
        .query_param("$select", false, "Fields to return")
        .query_param("$orderby", false, "Sort order")
        .handler(upstreams::list)
        .json_response_with_schema::<UpstreamListDto>(openapi, StatusCode::OK, "Upstream page")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.upstreams.get")
        .summary("Get an upstream")
        .description("Fetch one upstream owned by the calling tenant.")
        .tag(MANAGEMENT_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Upstream identifier")
        .handler(upstreams::get)
        .json_response_with_schema::<UpstreamDto>(openapi, StatusCode::OK, "The upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.upstreams.replace")
        .summary("Replace an upstream")
        .description("Full replacement; omitted optional fields are cleared.")
        .tag(MANAGEMENT_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Upstream identifier")
        .json_request::<UpstreamWriteDto>(openapi, "Replacement upstream definition")
        .handler(upstreams::replace)
        .json_response_with_schema::<UpstreamDto>(openapi, StatusCode::OK, "Replaced upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    OperationBuilder::delete("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.upstreams.delete")
        .summary("Delete an upstream")
        .description("Delete an upstream and every route beneath it.")
        .tag(MANAGEMENT_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Upstream identifier")
        .handler(upstreams::delete)
        .no_content_response(StatusCode::NO_CONTENT, "Upstream deleted")
        .standard_errors(openapi)
        .register(router, openapi)
}

fn register_routes_api(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    router = OperationBuilder::post("/oagw/v1/routes")
        .operation_id("oagw.routes.create")
        .summary("Create a route")
        .description("Register a match rule on an upstream.")
        .tag(MANAGEMENT_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .json_request::<RouteCreateDto>(openapi, "Route definition")
        .handler(route_handlers::create)
        .json_response_with_schema::<RouteDto>(openapi, StatusCode::CREATED, "Created route")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes")
        .operation_id("oagw.routes.list")
        .summary("List routes")
        .description("List the calling tenant's routes.")
        .tag(MANAGEMENT_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .query_param(
            "$top",
            false,
            "Maximum results, default 50 and capped at 100",
        )
        .query_param("$skip", false, "Offset into the result set")
        .query_param("$filter", false, "OData filter expression")
        .query_param("$select", false, "Fields to return")
        .query_param("$orderby", false, "Sort order")
        .handler(route_handlers::list)
        .json_response_with_schema::<RouteListDto>(openapi, StatusCode::OK, "Route page")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes/{id}")
        .operation_id("oagw.routes.get")
        .summary("Get a route")
        .description("Fetch one route owned by the calling tenant.")
        .tag(MANAGEMENT_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Route identifier")
        .handler(route_handlers::get)
        .json_response_with_schema::<RouteDto>(openapi, StatusCode::OK, "The route")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/routes/{id}")
        .operation_id("oagw.routes.replace")
        .summary("Replace a route")
        .description("Full replacement; the upstream reference is immutable.")
        .tag(MANAGEMENT_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Route identifier")
        .json_request::<RouteReplaceDto>(openapi, "Replacement route definition")
        .handler(route_handlers::replace)
        .json_response_with_schema::<RouteDto>(openapi, StatusCode::OK, "Replaced route")
        .standard_errors(openapi)
        .register(router, openapi);

    OperationBuilder::delete("/oagw/v1/routes/{id}")
        .operation_id("oagw.routes.delete")
        .summary("Delete a route")
        .description("Delete one route.")
        .tag(MANAGEMENT_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Route identifier")
        .handler(route_handlers::delete)
        .no_content_response(StatusCode::NO_CONTENT, "Route deleted")
        .standard_errors(openapi)
        .register(router, openapi)
}

fn register_plugins(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    router = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.plugins.create")
        .summary("Create a plugin")
        .description("Register a custom plugin. Plugins are immutable once created.")
        .tag(MANAGEMENT_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .json_request::<PluginCreateDto>(openapi, "Plugin definition")
        .handler(plugins::create)
        .json_response_with_schema::<PluginDto>(openapi, StatusCode::CREATED, "Created plugin")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins")
        .operation_id("oagw.plugins.list")
        .summary("List plugins")
        .description("List the calling tenant's plugins.")
        .tag(MANAGEMENT_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .query_param(
            "$top",
            false,
            "Maximum results, default 50 and capped at 100",
        )
        .query_param("$skip", false, "Offset into the result set")
        .query_param("$filter", false, "OData filter expression")
        .query_param("$select", false, "Fields to return")
        .handler(plugins::list)
        .json_response_with_schema::<PluginListDto>(openapi, StatusCode::OK, "Plugin page")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{id}")
        .operation_id("oagw.plugins.get")
        .summary("Get a plugin")
        .description("Fetch one plugin, including its stored source.")
        .tag(MANAGEMENT_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Plugin identifier")
        .handler(plugins::get)
        .json_response_with_schema::<PluginDto>(openapi, StatusCode::OK, "The plugin")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{id}/source")
        .operation_id("oagw.plugins.get_source")
        .summary("Get a plugin's source")
        .description("Fetch the plugin source verbatim.")
        .tag(MANAGEMENT_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Plugin identifier")
        .handler(plugins::get_source)
        .text_response(StatusCode::OK, "The plugin source", "text/plain")
        .standard_errors(openapi)
        .register(router, openapi);

    OperationBuilder::delete("/oagw/v1/plugins/{id}")
        .operation_id("oagw.plugins.delete")
        .summary("Delete a plugin")
        .description("Delete a plugin that nothing references.")
        .tag(MANAGEMENT_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Plugin identifier")
        .handler(plugins::delete)
        .no_content_response(StatusCode::NO_CONTENT, "Plugin deleted")
        .standard_errors(openapi)
        .register(router, openapi)
}

/// Register the proxy endpoint for every method the schema permits.
///
/// Each method is registered separately so the gateway's authentication policy,
/// which is keyed on method and path template, recognises all of them.
fn register_proxy(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    for (index, path) in ["/oagw/v1/proxy/{alias}", "/oagw/v1/proxy/{alias}/{*rest}"]
        .into_iter()
        .enumerate()
    {
        let with_suffix = index == 1;
        router = register_proxy_methods(router, openapi, path, with_suffix);
    }
    router
}

fn register_proxy_methods(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
    path: &str,
    with_suffix: bool,
) -> Router {
    let suffix_tag = if with_suffix { "path" } else { "root" };
    for method in ["get", "post", "put", "patch", "delete"] {
        let builder = match method {
            "post" => OperationBuilder::post(path),
            "put" => OperationBuilder::put(path),
            "patch" => OperationBuilder::patch(path),
            "delete" => OperationBuilder::delete(path),
            _ => OperationBuilder::get(path),
        };
        let builder = builder
            .operation_id(format!("oagw.proxy.{suffix_tag}.{method}"))
            .summary("Proxy a request to an upstream")
            .description(
                "Forward the request to the upstream named by the alias. Plain responses, \
                 server-sent-event streams and WebSocket upgrades are all relayed.",
            )
            .tag(PROXY_TAG)
            .authenticated()
            .require_license_features::<License>([])
            .path_param("alias", "Upstream alias");
        let builder = if with_suffix {
            builder.path_param("rest", "Path suffix appended to the route path")
        } else {
            builder
        };
        router = if with_suffix {
            builder
                .handler(proxy::proxy_with_path)
                .json_response(StatusCode::OK, "Relayed upstream response")
                .standard_errors(openapi)
                .register(router, openapi)
        } else {
            builder
                .handler(proxy::proxy_root)
                .json_response(StatusCode::OK, "Relayed upstream response")
                .standard_errors(openapi)
                .register(router, openapi)
        };
    }
    register_proxy_preflight(router, openapi, path, with_suffix, suffix_tag)
}

/// Register the preflight method for a proxy path.
///
/// A cross-origin preflight carries no credentials, so the route is anonymous.
/// `OperationBuilder::handler` maps only the five body-carrying methods, so the
/// method router is supplied directly.
fn register_proxy_preflight(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    path: &str,
    with_suffix: bool,
    suffix_tag: &str,
) -> Router {
    let builder = OperationBuilder::new(http::Method::OPTIONS, path)
        .operation_id(format!("oagw.proxy.{suffix_tag}.options"))
        .summary("Answer a cross-origin preflight")
        .description("Answer a preflight permissively, without resolving an upstream or a tenant.")
        .tag(PROXY_TAG)
        .anonymous()
        .path_param("alias", "Upstream alias");
    let builder = if with_suffix {
        builder.path_param("rest", "Path suffix appended to the route path")
    } else {
        builder
    };
    if with_suffix {
        builder
            .method_router(options(proxy::preflight_with_path))
            .no_content_response(StatusCode::NO_CONTENT, "Preflight accepted")
            .register(router, openapi)
    } else {
        builder
            .method_router(options(proxy::preflight_root))
            .no_content_response(StatusCode::NO_CONTENT, "Preflight accepted")
            .register(router, openapi)
    }
}
// @cpt-end:cpt-cf-oagw-dod-gear-foundation-router-mount:p1:inst-router
