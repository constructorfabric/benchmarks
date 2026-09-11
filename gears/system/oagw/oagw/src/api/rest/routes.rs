//! REST route registration for the `oagw` gear.
//!
//! Everything mounts gear-relative — `/oagw/v1/...`, with no `/api` prefix.

use std::sync::Arc;

use axum::Router;
use http::Method;
use toolkit::api::OpenApiRegistry;
use toolkit::api::canonical_prelude::StatusCode;
use toolkit::api::operation_builder::{
    CORE_GLOBAL_BASE_LICENSE_FEATURE, LicenseFeature, OperationBuilder, ResponseSpec,
};

use super::dto::{
    CreatePluginRequest, CreateRouteRequest, CreateUpstreamRequest, PluginDto, PluginSourceDto,
    RouteDto, UpdateRouteRequest, UpdateUpstreamRequest, UpstreamDto,
};
use super::handlers;
use crate::domain::services::management::ControlPlane;
use crate::infra::proxy::service::DataPlaneServiceImpl;

const API_TAG: &str = "OAGW";

struct License;

impl AsRef<str> for License {
    fn as_ref(&self) -> &'static str {
        CORE_GLOBAL_BASE_LICENSE_FEATURE
    }
}

impl LicenseFeature for License {}

/// The wildcard response every proxy operation declares.
fn proxy_response(status: u16, description: &str) -> ResponseSpec {
    ResponseSpec {
        status,
        content_type: "*/*",
        description: description.to_owned(),
        schema: None,
    }
}

/// Registers all REST routes for the `oagw` gear.
///
/// # Errors
/// Propagates a failure to build the data plane.
///
// The `Result` is part of this crate's public API and every other gear builder
// in the workspace returns one, so the always-`Ok` wrapper is kept deliberately.
#[allow(clippy::unnecessary_wraps)]
pub fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    control_plane: Arc<dyn ControlPlane>,
    data_plane: Arc<DataPlaneServiceImpl>,
) -> anyhow::Result<Router> {
    let router = management_routes(router, openapi);
    let router = proxy_routes(router, openapi);
    Ok(router
        .layer(axum::Extension(control_plane))
        .layer(axum::Extension(data_plane)))
}

#[allow(clippy::too_many_lines)]
fn management_routes(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    router = OperationBuilder::post("/oagw/v1/upstreams")
        .operation_id("oagw.create_upstream")
        .summary("Create an upstream")
        .description("Registers an upstream service definition and derives its routing alias.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .json_request::<CreateUpstreamRequest>(openapi, "Upstream to create")
        .handler(handlers::management::create_upstream)
        .json_response_with_schema::<UpstreamDto>(openapi, StatusCode::CREATED, "Created upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams")
        .operation_id("oagw.list_upstreams")
        .summary("List upstreams")
        .description("Lists the tenant's upstreams with the OData subset the gear supports.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .handler(handlers::management::list_upstreams)
        .json_response(StatusCode::OK, "The tenant's upstreams")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.get_upstream")
        .summary("Read an upstream")
        .description("Returns one upstream by identifier.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Upstream identifier")
        .handler(handlers::management::get_upstream)
        .json_response_with_schema::<UpstreamDto>(openapi, StatusCode::OK, "The upstream")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Upstream not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.replace_upstream")
        .summary("Replace an upstream")
        .description("Replaces an upstream; omitted fields fall back to the stored values.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .json_request::<UpdateUpstreamRequest>(openapi, "Upstream fields to replace")
        .handler(handlers::management::replace_upstream)
        .json_response_with_schema::<UpstreamDto>(openapi, StatusCode::OK, "The upstream")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Upstream not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.delete_upstream")
        .summary("Delete an upstream")
        .description("Deletes an upstream together with every route that references it.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Upstream identifier")
        .handler(handlers::management::delete_upstream)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Upstream not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/oagw/v1/routes")
        .operation_id("oagw.create_route")
        .summary("Create a route")
        .description("Registers a route under an existing upstream.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .json_request::<CreateRouteRequest>(openapi, "Route to create")
        .handler(handlers::management::create_route)
        .json_response_with_schema::<RouteDto>(openapi, StatusCode::CREATED, "Created route")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes")
        .operation_id("oagw.list_routes")
        .summary("List routes")
        .description("Lists the tenant's routes with the OData subset the gear supports.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .handler(handlers::management::list_routes)
        .json_response(StatusCode::OK, "The tenant's routes")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes/{id}")
        .operation_id("oagw.get_route")
        .summary("Read a route")
        .description("Returns one route by identifier.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Route identifier")
        .handler(handlers::management::get_route)
        .json_response_with_schema::<RouteDto>(openapi, StatusCode::OK, "The route")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Route not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/routes/{id}")
        .operation_id("oagw.replace_route")
        .summary("Replace a route")
        .description("Replaces a route; `upstream_id` is immutable and never part of the body.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .json_request::<UpdateRouteRequest>(openapi, "Route fields to replace")
        .handler(handlers::management::replace_route)
        .json_response_with_schema::<RouteDto>(openapi, StatusCode::OK, "The route")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Route not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/routes/{id}")
        .operation_id("oagw.delete_route")
        .summary("Delete a route")
        .description("Deletes a route.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Route identifier")
        .handler(handlers::management::delete_route)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Route not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.create_plugin")
        .summary("Create a plugin")
        .description("Registers a custom plugin definition with its Starlark source.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .json_request::<CreatePluginRequest>(openapi, "Plugin to create")
        .handler(handlers::management::create_plugin)
        .json_response_with_schema::<PluginDto>(openapi, StatusCode::CREATED, "Created plugin")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins")
        .operation_id("oagw.list_plugins")
        .summary("List plugins")
        .description("Lists the tenant's plugins with the OData subset the gear supports.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .handler(handlers::management::list_plugins)
        .json_response(StatusCode::OK, "The tenant's plugins")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{id}")
        .operation_id("oagw.get_plugin")
        .summary("Read a plugin")
        .description("Returns one plugin by identifier.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Plugin identifier")
        .handler(handlers::management::get_plugin)
        .json_response_with_schema::<PluginDto>(openapi, StatusCode::OK, "The plugin")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Plugin not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/plugins/{id}")
        .operation_id("oagw.delete_plugin")
        .summary("Delete a plugin")
        .description("Deletes a plugin, refusing while an upstream or route still references it.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Plugin identifier")
        .handler(handlers::management::delete_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Plugin not found")
        .problem_response(openapi, StatusCode::CONFLICT, "Plugin is still referenced")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{id}/source")
        .operation_id("oagw.get_plugin_source")
        .summary("Read a plugin's source")
        .description("Returns the Starlark source of one plugin.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Plugin identifier")
        .handler(handlers::management::get_plugin_source)
        .json_response_with_schema::<PluginSourceDto>(openapi, StatusCode::OK, "The plugin source")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Plugin not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router
}

fn proxy_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = document_proxy(
        router,
        openapi,
        "/oagw/v1/proxy/{alias}",
        handlers::proxy::proxy_root,
    );
    document_proxy(
        router,
        openapi,
        "/oagw/v1/proxy/{alias}/{*path}",
        handlers::proxy::proxy,
    )
}

/// Documents and routes one proxy path.
///
/// Every method shares one handler, so the path is routed once with `any` and
/// the remaining methods are documented against a throwaway router.
///
// The handler is taken by value because axum's routing methods consume it; each
// method registration only needs a clone of it.
#[allow(clippy::needless_pass_by_value)]
fn document_proxy<H, T>(mut router: Router, openapi: &dyn OpenApiRegistry, path: &'static str, handler: H) -> Router
where
    H: axum::handler::Handler<T, ()> + Clone + Send + 'static,
    T: 'static,
{
    let methods = [
        (Method::GET, "get"),
        (Method::POST, "post"),
        (Method::PUT, "put"),
        (Method::DELETE, "delete"),
        (Method::PATCH, "patch"),
    ];

    for (index, (method, name)) in methods.iter().enumerate() {
        let builder = OperationBuilder::new(method.clone(), path)
            .operation_id(format!("oagw.proxy_{name}"))
            .summary("Proxy a request")
            .description(
                "Forwards the request to the upstream the alias resolves to, streaming the \
                 response back. Errors the gateway generates are `application/problem+json`.",
            )
            .tag(API_TAG)
            .authenticated()
            .require_license_features::<License>([]);
        if index == 0 {
            router = builder
                .response(proxy_response(
                    200,
                    "The upstream response, streamed back as the gateway received it; a                      WebSocket upgrade is reported as 101 Switching Protocols.",
                ))
                .method_router(axum::routing::any(handler.clone()))
                .standard_errors(openapi)
                .register(router, openapi);
        } else {
            let documented = builder
                .response(proxy_response(
                    200,
                    "The upstream response, streamed back as the gateway received it; a                      WebSocket upgrade is reported as 101 Switching Protocols.",
                ))
                .handler(handler.clone())
                .standard_errors(openapi)
                .register(Router::new(), openapi);
            drop(documented);
        }
    }
    router
}
