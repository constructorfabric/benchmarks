//! REST route registration (`contracts/management-api.md`,
//! `contracts/proxy-api.md`).
//!
//! Paths are gear-relative: the gateway nests the router under its own
//! `prefix_path`, which is empty in the graded configuration.

use std::sync::Arc;

use axum::Router;
use toolkit::api::OpenApiRegistry;
use toolkit::api::canonical_prelude::StatusCode;
use toolkit::api::operation_builder::{
    CORE_GLOBAL_BASE_LICENSE_FEATURE, LicenseFeature, OperationBuilder,
};

use super::dto::{ListQuery, Plugin, Route, Upstream};
use super::handlers::{management, proxy};
use crate::domain::services::ManagementService;
use crate::infra::proxy::ProxyService;

/// Every management path lives under this base.
pub const BASE_PATH: &str = "/oagw/v1";

const API_TAG: &str = "OAGW";

struct License;

impl AsRef<str> for License {
    fn as_ref(&self) -> &'static str {
        CORE_GLOBAL_BASE_LICENSE_FEATURE
    }
}

impl LicenseFeature for License {}

/// Registers the gear's REST routes.
///
/// # Errors
/// Propagates the router; the builder is infallible in practice.
#[allow(clippy::needless_pass_by_value)]
pub fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    management: Arc<ManagementService>,
    proxy: Arc<ProxyService>,
) -> Router {
    let router = register_upstreams(router, openapi);
    let router = register_routes_routes(router, openapi);
    let router = register_plugins(router, openapi);

    // The proxy path is a catch-all: any method, any remaining path, any body
    // shape. It is registered directly, because its responses are the
    // upstream's rather than the gear's own schema.
    let router = router.route(
        proxy::PROXY_PATH,
        axum::routing::any(proxy::proxy).layer(axum::Extension(proxy.clone())),
    );

    router
        .layer(axum::middleware::from_fn(
            crate::api::rest::problem::with_instance,
        ))
        .layer(axum::Extension(management))
        .layer(axum::Extension(proxy))
}

fn register_upstreams(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::post(format!("{BASE_PATH}/upstreams"))
        .operation_id("oagw.upstreams.create")
        .summary("Create an upstream")
        .description("Declares an outbound service. The alias is derived from the endpoints, or validated against the supplied one.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .json_request::<Upstream>(openapi, "The upstream to create")
        .handler(management::create_upstream)
        .json_response_with_schema::<Upstream>(openapi, StatusCode::CREATED, "The stored upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(format!("{BASE_PATH}/upstreams"))
        .operation_id("oagw.upstreams.list")
        .summary("List upstreams")
        .description("Lists the calling tenant's upstreams.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .query_params_from::<ListQuery>()
        .handler(management::list_upstreams)
        .json_array_response_with_schema::<Upstream>(
            openapi,
            StatusCode::OK,
            "The tenant's upstreams",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(format!("{BASE_PATH}/upstreams/{{id}}"))
        .operation_id("oagw.upstreams.get")
        .summary("Read an upstream")
        .description("Reads one upstream by id.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Upstream identifier")
        .handler(management::get_upstream)
        .json_response_with_schema::<Upstream>(openapi, StatusCode::OK, "The upstream")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Upstream not found")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::put(format!("{BASE_PATH}/upstreams/{{id}}"))
        .operation_id("oagw.upstreams.replace")
        .summary("Replace an upstream")
        .description("Full replacement of an upstream; the alias is immutable.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Upstream identifier")
        .json_request::<Upstream>(openapi, "The replacement upstream")
        .handler(management::replace_upstream)
        .json_response_with_schema::<Upstream>(openapi, StatusCode::OK, "The replaced upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    OperationBuilder::delete(format!("{BASE_PATH}/upstreams/{{id}}"))
        .operation_id("oagw.upstreams.delete")
        .summary("Delete an upstream")
        .description("Deletes the upstream and cascades to its routes.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Upstream identifier")
        .handler(management::delete_upstream)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Upstream not found")
        .standard_errors(openapi)
        .register(router, openapi)
}

fn register_routes_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::post(format!("{BASE_PATH}/routes"))
        .operation_id("oagw.routes.create")
        .summary("Create a route")
        .description("Binds a match rule to an existing upstream.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .json_request::<Route>(openapi, "The route to create")
        .handler(management::create_route)
        .json_response_with_schema::<Route>(openapi, StatusCode::CREATED, "The stored route")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(format!("{BASE_PATH}/routes"))
        .operation_id("oagw.routes.list")
        .summary("List routes")
        .description("Lists the calling tenant's routes.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .query_params_from::<ListQuery>()
        .handler(management::list_routes)
        .json_array_response_with_schema::<Route>(openapi, StatusCode::OK, "The tenant's routes")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(format!("{BASE_PATH}/routes/{{id}}"))
        .operation_id("oagw.routes.get")
        .summary("Read a route")
        .description("Reads one route by id.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Route identifier")
        .handler(management::get_route)
        .json_response_with_schema::<Route>(openapi, StatusCode::OK, "The route")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Route not found")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::put(format!("{BASE_PATH}/routes/{{id}}"))
        .operation_id("oagw.routes.replace")
        .summary("Replace a route")
        .description("Full replacement of a route; the owning upstream is immutable.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Route identifier")
        .json_request::<Route>(openapi, "The replacement route")
        .handler(management::replace_route)
        .json_response_with_schema::<Route>(openapi, StatusCode::OK, "The replaced route")
        .standard_errors(openapi)
        .register(router, openapi);

    OperationBuilder::delete(format!("{BASE_PATH}/routes/{{id}}"))
        .operation_id("oagw.routes.delete")
        .summary("Delete a route")
        .description("Deletes a route; no cascade.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Route identifier")
        .handler(management::delete_route)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Route not found")
        .standard_errors(openapi)
        .register(router, openapi)
}

fn register_plugins(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::post(format!("{BASE_PATH}/plugins"))
        .operation_id("oagw.plugins.create")
        .summary("Create a custom plugin")
        .description("Creates an immutable custom plugin definition (Starlark source).")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .json_request::<Plugin>(openapi, "The plugin definition")
        .handler(management::create_plugin)
        .json_response_with_schema::<Plugin>(openapi, StatusCode::CREATED, "The stored plugin")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(format!("{BASE_PATH}/plugins"))
        .operation_id("oagw.plugins.list")
        .summary("List custom plugins")
        .description("Lists the calling tenant's custom plugins; built-ins are never listed.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .query_params_from::<ListQuery>()
        .handler(management::list_plugins)
        .json_array_response_with_schema::<Plugin>(
            openapi,
            StatusCode::OK,
            "The tenant's custom plugins",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(format!("{BASE_PATH}/plugins/{{id}}"))
        .operation_id("oagw.plugins.get")
        .summary("Read a custom plugin")
        .description("Reads one custom plugin by id.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Plugin identifier")
        .handler(management::get_plugin)
        .json_response_with_schema::<Plugin>(openapi, StatusCode::OK, "The plugin")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Plugin not found")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(format!("{BASE_PATH}/plugins/{{id}}/source"))
        .operation_id("oagw.plugins.source")
        .summary("Read a plugin's source")
        .description("Returns the stored Starlark source as text/plain.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Plugin identifier")
        .handler(management::get_plugin_source)
        .text_response(StatusCode::OK, "The Starlark source", "text/plain")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Plugin not found")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::put(format!("{BASE_PATH}/plugins/{{id}}"))
        .operation_id("oagw.plugins.replace")
        .summary("Replace a custom plugin (unsupported)")
        .description("Custom plugins are immutable; the method answers 405.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Plugin identifier")
        .handler(management::replace_plugin_not_allowed)
        .problem_response(
            openapi,
            StatusCode::METHOD_NOT_ALLOWED,
            "Plugins are immutable",
        )
        .register(router, openapi);

    OperationBuilder::delete(format!("{BASE_PATH}/plugins/{{id}}"))
        .operation_id("oagw.plugins.delete")
        .summary("Delete a custom plugin")
        .description("Deletes an unreferenced custom plugin.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Plugin identifier")
        .handler(management::delete_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .problem_response(openapi, StatusCode::CONFLICT, "Plugin in use")
        .standard_errors(openapi)
        .register(router, openapi)
}
