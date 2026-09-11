//! Router construction: the management API and the proxy API.
//!
//! Every path here is gear-relative. The host runtime nests the returned router
//! under its own prefix, which is empty in the graded configuration, so the
//! gear serves `/oagw/v1/...` as published.
//!
//! Management operations go through [`OperationBuilder`] so the host sees them
//! in its OpenAPI registry and its authentication policy. The proxy endpoint is
//! method-agnostic — `{METHOD} /proxy/{alias}[/{path}]` — so it is routed once
//! with `any` and each HTTP method is described by its own operation spec.

use axum::Router;
use axum::routing::any;
use http::Method;
use toolkit::api::OpenApiRegistry;
use toolkit::api::canonical_prelude::StatusCode;
use toolkit::api::operation_builder::{
    CORE_GLOBAL_BASE_LICENSE_FEATURE, LicenseFeature, OperationBuilder, OperationSpec, ParamLocation,
    ParamSpec, ResponseSchema, ResponseSpec, VendorExtensions,
};

use crate::api::rest::handlers::{plugins, proxy, routes, upstreams};

const API_TAG: &str = "OAGW";

struct License;

impl AsRef<str> for License {
    fn as_ref(&self) -> &'static str {
        CORE_GLOBAL_BASE_LICENSE_FEATURE
    }
}

impl LicenseFeature for License {}

/// Builds the gear's router.
#[allow(clippy::needless_pass_by_value)]
pub fn router(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    // -- upstreams ------------------------------------------------------
    router = OperationBuilder::post("/oagw/v1/upstreams")
        .operation_id("oagw.create_upstream")
        .summary("Create an upstream")
        .description("Registers an outbound upstream owned by the caller's tenant.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .handler(upstreams::create_upstream)
        .json_response(StatusCode::CREATED, "The created upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams")
        .operation_id("oagw.list_upstreams")
        .summary("List upstreams")
        .description("Lists the upstreams visible to the caller's tenant.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .query_param("filter", false, "Alias substring filter")
        .query_param("orderby", false, "Alias ordering")
        .query_param("top", false, "Page size")
        .query_param("skip", false, "Page offset")
        .handler(upstreams::list_upstreams)
        .json_response(StatusCode::OK, "A page of upstreams")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.get_upstream")
        .summary("Read an upstream")
        .description("Reads one upstream by its identifier.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Upstream identifier")
        .handler(upstreams::get_upstream)
        .json_response(StatusCode::OK, "The upstream")
        .problem_response(openapi, StatusCode::NOT_FOUND, "No such upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.update_upstream")
        .summary("Replace an upstream")
        .description("Replaces an upstream's configuration.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Upstream identifier")
        .handler(upstreams::update_upstream)
        .json_response(StatusCode::OK, "The replaced upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.delete_upstream")
        .summary("Delete an upstream")
        .description("Deletes an upstream and every route bound to it.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Upstream identifier")
        .handler(upstreams::delete_upstream)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .problem_response(openapi, StatusCode::NOT_FOUND, "No such upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    // -- routes ---------------------------------------------------------
    router = OperationBuilder::post("/oagw/v1/routes")
        .operation_id("oagw.create_route")
        .summary("Create a route")
        .description("Binds a match rule and a configuration overlay to an upstream.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .handler(routes::create_route)
        .json_response(StatusCode::CREATED, "The created route")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes")
        .operation_id("oagw.list_routes")
        .summary("List routes")
        .description("Lists the routes visible to the caller's tenant.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .query_param("filter", false, "Upstream-id substring filter")
        .query_param("orderby", false, "Path ordering")
        .query_param("top", false, "Page size")
        .query_param("skip", false, "Page offset")
        .handler(routes::list_routes)
        .json_response(StatusCode::OK, "A page of routes")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes/{id}")
        .operation_id("oagw.get_route")
        .summary("Read a route")
        .description("Reads one route by its identifier.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Route identifier")
        .handler(routes::get_route)
        .json_response(StatusCode::OK, "The route")
        .problem_response(openapi, StatusCode::NOT_FOUND, "No such route")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/routes/{id}")
        .operation_id("oagw.update_route")
        .summary("Replace a route")
        .description("Replaces a route's match rule and configuration overlay.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Route identifier")
        .handler(routes::update_route)
        .json_response(StatusCode::OK, "The replaced route")
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
        .handler(routes::delete_route)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .problem_response(openapi, StatusCode::NOT_FOUND, "No such route")
        .standard_errors(openapi)
        .register(router, openapi);

    // -- plugins --------------------------------------------------------
    router = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.create_plugin")
        .summary("Create a plugin")
        .description("Records a custom plugin implementation for the caller's tenant.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .handler(plugins::create_plugin)
        .json_response(StatusCode::CREATED, "The created plugin")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins")
        .operation_id("oagw.list_plugins")
        .summary("List plugins")
        .description("Lists the plugins visible to the caller's tenant.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .query_param("filter", false, "Identifier substring filter")
        .query_param("orderby", false, "Identifier ordering")
        .query_param("top", false, "Page size")
        .query_param("skip", false, "Page offset")
        .handler(plugins::list_plugins)
        .json_response(StatusCode::OK, "A page of plugins")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{id}")
        .operation_id("oagw.get_plugin")
        .summary("Read a plugin")
        .description("Reads one plugin record by its identifier.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Plugin identifier")
        .handler(plugins::get_plugin)
        .json_response(StatusCode::OK, "The plugin record")
        .problem_response(openapi, StatusCode::NOT_FOUND, "No such plugin")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{id}/source")
        .operation_id("oagw.get_plugin_source")
        .summary("Read a plugin's source")
        .description("Returns the recorded source artifact of a custom plugin.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Plugin identifier")
        .handler(plugins::get_plugin_source)
        .json_response(StatusCode::OK, "The plugin source artifact")
        .problem_response(openapi, StatusCode::NOT_FOUND, "No such plugin")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/plugins/{id}")
        .operation_id("oagw.delete_plugin")
        .summary("Delete a plugin")
        .description("Deletes a custom plugin record.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Plugin identifier")
        .handler(plugins::delete_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .problem_response(openapi, StatusCode::NOT_FOUND, "No such plugin")
        .standard_errors(openapi)
        .register(router, openapi);

    // -- proxy ----------------------------------------------------------
    // One `MethodRouter` per path: the endpoint is method-agnostic, so every
    // HTTP method — including `OPTIONS` for CORS preflight — reaches the same
    // handler. Each method is then described to the OpenAPI registry so the
    // host's authentication policy covers the whole surface.
    let alias = "/oagw/v1/proxy/{alias}";
    let alias_path = "/oagw/v1/proxy/{alias}/{*path}";
    for method in [Method::GET, Method::POST, Method::PUT, Method::DELETE, Method::PATCH] {
        describe_proxy(openapi, method.clone(), alias, "proxy");
        describe_proxy(openapi, method, alias_path, "proxy_with_path");
    }

    router
        .route(alias, any(proxy::proxy))
        .route(alias_path, any(proxy::proxy_with_path))
}

/// Describes one proxy operation to the OpenAPI registry.
fn describe_proxy(openapi: &dyn OpenApiRegistry, method: Method, path: &str, kind: &str) {
    let id = format!("oagw.{kind}.{}", method.as_str().to_ascii_lowercase());
    openapi.register_operation(&OperationSpec {
        method: method.clone(),
        path: path.to_string(),
        operation_id: Some(id.clone()),
        summary: Some("Forward a request to an upstream".to_string()),
        description: Some(
            "Resolves the alias down the caller's tenant chain and forwards the request to the \
             winning endpoint, applying plugins, rate limiting and header transformation."
                .to_string(),
        ),
        tags: vec![API_TAG.to_string()],
        params: vec![ParamSpec {
            name: "alias".to_string(),
            location: ParamLocation::Path,
            required: true,
            description: Some("The upstream alias to reach".to_string()),
            param_type: "string".to_string(),
            array: false,
        }],
        request_body: None,
        responses: vec![ResponseSpec {
            status: StatusCode::OK.as_u16(),
            content_type: "application/json",
            description: "The upstream response".to_string(),
            schema: Some(ResponseSchema::Ref {
                schema_name: "Problem".to_string(),
            }),
        }],
        handler_id: id,
        authenticated: true,
        exposed: false,
        rate_limit: None,
        allowed_request_content_types: None,
        vendor_extensions: VendorExtensions::default(),
        license_requirement: Some(toolkit::api::operation_builder::LicenseReqSpec {
            license_names: vec![CORE_GLOBAL_BASE_LICENSE_FEATURE.to_string()],
        }),
    });
}

/// The routes documented in the OpenAPI registry, for the docs generator.
pub fn documented() -> Vec<(&'static str, &'static str)> {
    vec![
        ("POST", "/oagw/v1/upstreams"),
        ("GET", "/oagw/v1/upstreams"),
        ("GET", "/oagw/v1/upstreams/{id}"),
        ("PUT", "/oagw/v1/upstreams/{id}"),
        ("DELETE", "/oagw/v1/upstreams/{id}"),
        ("POST", "/oagw/v1/routes"),
        ("GET", "/oagw/v1/routes"),
        ("GET", "/oagw/v1/routes/{id}"),
        ("PUT", "/oagw/v1/routes/{id}"),
        ("DELETE", "/oagw/v1/routes/{id}"),
        ("POST", "/oagw/v1/plugins"),
        ("GET", "/oagw/v1/plugins"),
        ("GET", "/oagw/v1/plugins/{id}"),
        ("DELETE", "/oagw/v1/plugins/{id}"),
        ("GET", "/oagw/v1/plugins/{id}/source"),
    ]
}
