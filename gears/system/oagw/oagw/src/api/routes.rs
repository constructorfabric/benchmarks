//! REST route registration for the OAGW gear.
//!
//! Every route is registered gear-relative (`/oagw/v1/...`); the gateway nests
//! the router under its own prefix.

use axum::{Extension, Router};
use toolkit::api::{OpenApiRegistry, canonical_prelude::StatusCode};
use toolkit::api::operation_builder::OperationBuilder;

use super::dto::{PluginDto, RouteDto, UpstreamDto};
use super::handlers::{self, Services};
use crate::infra::proxy::service::DataPlaneService;
use crate::infra::storage::MemoryStore;
use crate::domain::service::ControlPlaneService;

/// Concrete control-plane service type.
pub type Control = ControlPlaneService<MemoryStore, MemoryStore, MemoryStore>;
/// Concrete data-plane service type.
pub type Data = DataPlaneService<MemoryStore, MemoryStore, MemoryStore>;

const PROXY_METHODS: [axum::http::Method; 6] = [
    axum::http::Method::GET,
    axum::http::Method::POST,
    axum::http::Method::PUT,
    axum::http::Method::PATCH,
    axum::http::Method::DELETE,
    axum::http::Method::HEAD,
];

/// Register every OAGW route.
#[allow(clippy::needless_pass_by_value)]
pub fn register_routes(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
    services: Services,
) -> Router {
    // -- Upstreams ---------------------------------------------------------
    router = OperationBuilder::post("/oagw/v1/upstreams")
        .operation_id("oagw.create_upstream")
        .summary("Create an upstream")
        .description("Create a new upstream endpoint pool. The alias is derived from the endpoints when possible.")
        .tag(super::TAG_UPSTREAM)
        .authenticated()
        .no_license_required()
        .handler(handlers::create_upstream)
        .json_response_with_schema::<UpstreamDto>(openapi, StatusCode::CREATED, "Upstream created")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams")
        .operation_id("oagw.list_upstreams")
        .summary("List upstreams")
        .description("List all upstreams owned by the calling tenant, including inherited ones.")
        .tag(super::TAG_UPSTREAM)
        .authenticated()
        .no_license_required()
        .handler(handlers::list_upstreams)
        .json_array_response_with_schema::<UpstreamDto>(
            openapi,
            StatusCode::OK,
            "List of upstreams",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.get_upstream")
        .summary("Get an upstream")
        .description("Retrieve a single upstream by identifier.")
        .tag(super::TAG_UPSTREAM)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream identifier")
        .handler(handlers::get_upstream)
        .json_response_with_schema::<UpstreamDto>(openapi, StatusCode::OK, "The upstream")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Upstream not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.update_upstream")
        .summary("Replace an upstream")
        .description("Replace an upstream configuration. The alias is immutable.")
        .tag(super::TAG_UPSTREAM)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream identifier")
        .handler(handlers::update_upstream)
        .json_response_with_schema::<UpstreamDto>(openapi, StatusCode::OK, "Updated upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.delete_upstream")
        .summary("Delete an upstream")
        .description("Delete an upstream and all routes that reference it.")
        .tag(super::TAG_UPSTREAM)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream identifier")
        .handler(handlers::delete_upstream)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .standard_errors(openapi)
        .register(router, openapi);

    // -- Routes ------------------------------------------------------------
    router = OperationBuilder::post("/oagw/v1/routes")
        .operation_id("oagw.create_route")
        .summary("Create a route")
        .description("Create a route that binds a match rule to an upstream.")
        .tag(super::TAG_ROUTE)
        .authenticated()
        .no_license_required()
        .handler(handlers::create_route)
        .json_response_with_schema::<RouteDto>(openapi, StatusCode::CREATED, "Route created")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes")
        .operation_id("oagw.list_routes")
        .summary("List routes")
        .description("List routes, optionally filtered by `upstream_id`.")
        .tag(super::TAG_ROUTE)
        .authenticated()
        .no_license_required()
        .query_param("upstream_id", false, "Filter by the referenced upstream")
        .handler(handlers::list_routes)
        .json_array_response_with_schema::<RouteDto>(openapi, StatusCode::OK, "List of routes")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes/{id}")
        .operation_id("oagw.get_route")
        .summary("Get a route")
        .description("Retrieve a single route by identifier.")
        .tag(super::TAG_ROUTE)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route identifier")
        .handler(handlers::get_route)
        .json_response_with_schema::<RouteDto>(openapi, StatusCode::OK, "The route")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Route not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/routes/{id}")
        .operation_id("oagw.update_route")
        .summary("Replace a route")
        .description("Replace the mutable parts of a route.")
        .tag(super::TAG_ROUTE)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route identifier")
        .handler(handlers::update_route)
        .json_response_with_schema::<RouteDto>(openapi, StatusCode::OK, "Updated route")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/routes/{id}")
        .operation_id("oagw.delete_route")
        .summary("Delete a route")
        .description("Delete a route.")
        .tag(super::TAG_ROUTE)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route identifier")
        .handler(handlers::delete_route)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .standard_errors(openapi)
        .register(router, openapi);

    // -- Plugins -----------------------------------------------------------
    router = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.create_plugin")
        .summary("Register a plugin")
        .description("Register a plugin source in the tenant plugin catalog.")
        .tag(super::TAG_PLUGIN)
        .authenticated()
        .no_license_required()
        .handler(handlers::create_plugin)
        .json_response_with_schema::<PluginDto>(openapi, StatusCode::CREATED, "Plugin registered")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins")
        .operation_id("oagw.list_plugins")
        .summary("List plugins")
        .description("List the plugins registered by the calling tenant.")
        .tag(super::TAG_PLUGIN)
        .authenticated()
        .no_license_required()
        .handler(handlers::list_plugins)
        .json_array_response_with_schema::<PluginDto>(openapi, StatusCode::OK, "List of plugins")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{id}")
        .operation_id("oagw.get_plugin")
        .summary("Get a plugin")
        .description("Retrieve a single plugin by identifier.")
        .tag(super::TAG_PLUGIN)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin identifier")
        .handler(handlers::get_plugin)
        .json_response_with_schema::<PluginDto>(openapi, StatusCode::OK, "The plugin")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Plugin not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{id}/source")
        .operation_id("oagw.get_plugin_source")
        .summary("Get plugin source")
        .description("Retrieve the plugin source code.")
        .tag(super::TAG_PLUGIN)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin identifier")
        .handler(handlers::get_plugin_source)
        .text_response(StatusCode::OK, "Plugin source", "text/x-python")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/plugins/{id}")
        .operation_id("oagw.delete_plugin")
        .summary("Delete a plugin")
        .description("Delete a plugin that is no longer bound to an upstream or route.")
        .tag(super::TAG_PLUGIN)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin identifier")
        .handler(handlers::delete_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .problem_response(openapi, StatusCode::CONFLICT, "Plugin still in use")
        .standard_errors(openapi)
        .register(router, openapi);

    // -- Data plane --------------------------------------------------------
    for method in PROXY_METHODS {
        let method_name = method.as_str().to_ascii_lowercase();
        router = proxy_operation(
            router,
            openapi,
            method.clone(),
            "/oagw/v1/proxy/{alias}",
            "oagw.proxy_root",
            &format!("Proxy a request to an upstream root ({method_name})"),
        );
        router = proxy_operation(
            router,
            openapi,
            method,
            "/oagw/v1/proxy/{alias}/{*path_suffix}",
            "oagw.proxy",
            &format!("Proxy a request to an upstream ({method_name})"),
        );
    }

    // CORS preflight never enters the `OpenAPI` document and never reaches the
    // upstream; see `ADR/0004-cors.md`.
    router = router
        .route(
            "/oagw/v1/proxy/{alias}",
            axum::routing::options(handlers::proxy_preflight),
        )
        .route(
            "/oagw/v1/proxy/{alias}/{*path_suffix}",
            axum::routing::options(handlers::proxy_preflight),
        );

    router.layer(Extension(services))
}

/// Register one proxy operation for `path`.
fn proxy_operation(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    method: axum::http::Method,
    path: &'static str,
    operation_id: &str,
    summary: &str,
) -> Router {
    let builder = match method {
        axum::http::Method::GET => OperationBuilder::get(path),
        axum::http::Method::POST => OperationBuilder::post(path),
        axum::http::Method::PUT => OperationBuilder::put(path),
        axum::http::Method::PATCH => OperationBuilder::patch(path),
        axum::http::Method::DELETE => OperationBuilder::delete(path),
        // `HEAD` has no convenience constructor and no dedicated axum routing
        // helper in `OperationBuilder::handler`; wire the method router directly.
        axum::http::Method::HEAD => {
            return OperationBuilder::<
                toolkit::api::operation_builder::Missing,
                toolkit::api::operation_builder::Missing,
                (),
            >::new(method, path)
            .operation_id(operation_id)
            .summary(summary)
            .description("Proxies the request to the upstream selected by alias.")
            .tag(super::TAG_PROXY)
            .authenticated()
            .no_license_required()
            .path_param("alias", "Upstream alias")
            .method_router(axum::routing::head(handlers::proxy))
            .text_response(StatusCode::OK, "Upstream response", "*/*")
            .standard_errors(openapi)
            .register(router, openapi);
        }
        _ => unreachable!("only proxy methods are listed"),
    };

    let builder = builder
        .operation_id(operation_id)
        .summary(summary)
        .description("Proxies the request to the upstream selected by alias.")
        .tag(super::TAG_PROXY)
        .authenticated()
        .no_license_required()
        .path_param("alias", "Upstream alias")
        // The suffix is derived from the request URI, so one handler serves
        // both the bare-alias and the path-suffix routes.
        .handler(handlers::proxy);
    let builder: toolkit::api::operation_builder::OperationBuilder<
        toolkit::api::operation_builder::Present,
        toolkit::api::operation_builder::Present,
        (),
        toolkit::api::operation_builder::AuthSet,
        toolkit::api::operation_builder::LicenseSet,
    > = builder
        .text_response(StatusCode::OK, "Upstream response", "*/*")
        .standard_errors(openapi);
    builder.register(router, openapi)
}
