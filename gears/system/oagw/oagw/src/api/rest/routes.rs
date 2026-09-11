//! Route registration for both OAGW surfaces.
//!
//! Paths are gear-relative (`/oagw/v1/...`): the gear never repeats the
//! host-level prefix, which the api-gateway adds when it nests this router
//! under its own `prefix_path`.
//!
//! The proxy paths take one `MethodRouter` each that answers every method,
//! because the data plane owns the whole exchange — request body, response
//! stream and upgrades — and has no DTO to describe. Each method is still
//! documented separately in the `OpenAPI` document, so a client reading the
//! catalog sees the verbs the gateway forwards.

use std::sync::Arc;

use axum::Router;
use toolkit::api::operation_builder::{
    OperationSpec, ParamLocation, ParamSpec, ResponseSpec, VendorExtensions,
};
use toolkit::api::{OpenApiRegistry, OperationBuilder};

use crate::api::rest::dto;
use crate::api::rest::handlers;
use crate::domain::services::control_plane::ControlPlaneService;
use crate::infra::proxy::service::GatewayService;

const TAG_MANAGEMENT: &str = "OAGW Management";
const TAG_PROXY: &str = "OAGW Data Plane";

const PROXY_ALIAS_PATH: &str = "/oagw/v1/proxy/{alias}";
const PROXY_PATH_WILDCARD: &str = "/oagw/v1/proxy/{alias}/{*path}";

/// Every method the data plane forwards.
const PROXY_METHODS: [http::Method; 7] = [
    http::Method::GET,
    http::Method::POST,
    http::Method::PUT,
    http::Method::PATCH,
    http::Method::DELETE,
    http::Method::HEAD,
    http::Method::OPTIONS,
];

/// Register every OAGW route on `router`.
#[allow(clippy::needless_pass_by_value)]
pub fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    control_plane: Arc<ControlPlaneService>,
    gateway: Arc<GatewayService>,
) -> Router {
    let router = register_upstream_routes(router, openapi);
    let router = register_route_routes(router, openapi);
    let router = register_plugin_routes(router, openapi);
    let router = register_proxy_routes(router, openapi);

    router
        .layer(axum::Extension(control_plane))
        .layer(axum::Extension(gateway))
}

fn register_upstream_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let listed = OperationBuilder::get("/oagw/v1/upstreams")
        .operation_id("oagw.list_upstreams")
        .summary("List upstreams")
        .description("List the upstream endpoint pools visible to the caller, OData filtered")
        .tag(TAG_MANAGEMENT)
        .authenticated()
        .no_license_required()
        .query_param("$top", false, "Page size, at most 100")
        .query_param("$skiptoken", false, "Opaque continuation token")
        .query_param("$filter", false, "OData v4 filter expression")
        .query_param("$orderby", false, "OData v4 orderby expression")
        .query_param("$select", false, "OData v4 select expression")
        .handler(handlers::upstreams::list_upstreams)
        .json_response_with_schema::<dto::UpstreamListDto>(
            openapi,
            http::StatusCode::OK,
            "The visible upstreams",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let created = OperationBuilder::post("/oagw/v1/upstreams")
        .operation_id("oagw.create_upstream")
        .summary("Create an upstream")
        .description("Register an upstream endpoint pool owned by the caller's tenant")
        .tag(TAG_MANAGEMENT)
        .authenticated()
        .no_license_required()
        .json_request::<dto::UpstreamCreateRequest>(openapi, "The upstream to register")
        .handler(handlers::upstreams::create_upstream)
        .json_response_with_schema::<dto::UpstreamDto>(
            openapi,
            http::StatusCode::CREATED,
            "The registered upstream",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(listed, openapi);

    let read = OperationBuilder::get("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.get_upstream")
        .summary("Read an upstream")
        .description("Read one upstream endpoint pool by identifier")
        .tag(TAG_MANAGEMENT)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream identifier")
        .handler(handlers::upstreams::get_upstream)
        .json_response_with_schema::<dto::UpstreamDto>(
            openapi,
            http::StatusCode::OK,
            "The upstream",
        )
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(created, openapi);

    let replaced = OperationBuilder::put("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.replace_upstream")
        .summary("Replace an upstream")
        .description("Replace an upstream endpoint pool; the alias is immutable")
        .tag(TAG_MANAGEMENT)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream identifier")
        .json_request::<dto::UpstreamCreateRequest>(openapi, "The replacement upstream")
        .handler(handlers::upstreams::replace_upstream)
        .json_response_with_schema::<dto::UpstreamDto>(
            openapi,
            http::StatusCode::OK,
            "The replaced upstream",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(read, openapi);

    OperationBuilder::delete("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.delete_upstream")
        .summary("Delete an upstream")
        .description("Delete an upstream that no route references")
        .tag(TAG_MANAGEMENT)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream identifier")
        .handler(handlers::upstreams::delete_upstream)
        .json_response(http::StatusCode::NO_CONTENT, "The upstream is gone")
        .error_401(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(replaced, openapi)
}

fn register_route_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let listed = OperationBuilder::get("/oagw/v1/routes")
        .operation_id("oagw.list_routes")
        .summary("List routes")
        .description("List the caller's routes, OData filtered")
        .tag(TAG_MANAGEMENT)
        .authenticated()
        .no_license_required()
        .query_param("$top", false, "Page size, at most 100")
        .query_param("$filter", false, "OData v4 filter expression")
        .query_param("$orderby", false, "OData v4 orderby expression")
        .query_param("$select", false, "OData v4 select expression")
        .handler(handlers::routes_api::list_routes)
        .json_response_with_schema::<dto::RouteListDto>(
            openapi,
            http::StatusCode::OK,
            "The caller's routes",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let created = OperationBuilder::post("/oagw/v1/routes")
        .operation_id("oagw.create_route")
        .summary("Create a route")
        .description("Bind a path and a method set to an upstream alias")
        .tag(TAG_MANAGEMENT)
        .authenticated()
        .no_license_required()
        .json_request::<dto::RouteCreateRequest>(openapi, "The route to create")
        .handler(handlers::routes_api::create_route)
        .json_response_with_schema::<dto::RouteDto>(
            openapi,
            http::StatusCode::CREATED,
            "The created route",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(listed, openapi);

    let read = OperationBuilder::get("/oagw/v1/routes/{id}")
        .operation_id("oagw.get_route")
        .summary("Read a route")
        .description("Read one route by identifier")
        .tag(TAG_MANAGEMENT)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route identifier")
        .handler(handlers::routes_api::get_route)
        .json_response_with_schema::<dto::RouteDto>(openapi, http::StatusCode::OK, "The route")
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(created, openapi);

    let replaced = OperationBuilder::put("/oagw/v1/routes/{id}")
        .operation_id("oagw.replace_route")
        .summary("Replace a route")
        .description("Replace a route in full")
        .tag(TAG_MANAGEMENT)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route identifier")
        .json_request::<dto::RouteCreateRequest>(openapi, "The replacement route")
        .handler(handlers::routes_api::replace_route)
        .json_response_with_schema::<dto::RouteDto>(
            openapi,
            http::StatusCode::OK,
            "The replaced route",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(read, openapi);

    OperationBuilder::delete("/oagw/v1/routes/{id}")
        .operation_id("oagw.delete_route")
        .summary("Delete a route")
        .description("Delete a route and the plugin bindings it carries")
        .tag(TAG_MANAGEMENT)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route identifier")
        .handler(handlers::routes_api::delete_route)
        .json_response(http::StatusCode::NO_CONTENT, "The route is gone")
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(replaced, openapi)
}

fn register_plugin_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let filed = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.create_plugin")
        .summary("File a plugin")
        .description("Store a tenant-defined plugin in the catalog")
        .tag(TAG_MANAGEMENT)
        .authenticated()
        .no_license_required()
        .handler(handlers::plugins::create_plugin)
        .json_response_with_schema::<dto::PluginDto>(
            openapi,
            http::StatusCode::CREATED,
            "The filed plugin",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let listed = OperationBuilder::get("/oagw/v1/plugins")
        .operation_id("oagw.list_plugins")
        .summary("List plugins")
        .description("The shipped catalog plus every custom plugin in scope")
        .tag(TAG_MANAGEMENT)
        .authenticated()
        .no_license_required()
        .handler(handlers::plugins::list_plugins)
        .json_response_with_schema::<dto::PluginListDto>(
            openapi,
            http::StatusCode::OK,
            "The plugin catalog",
        )
        .error_401(openapi)
        .error_500(openapi)
        .register(filed, openapi);

    let read = OperationBuilder::get("/oagw/v1/plugins/{id}")
        .operation_id("oagw.get_plugin")
        .summary("Read a plugin")
        .description("Read one plugin catalog entry by identifier")
        .tag(TAG_MANAGEMENT)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin identifier")
        .handler(handlers::plugins::get_plugin)
        .json_response_with_schema::<dto::PluginDto>(openapi, http::StatusCode::OK, "The plugin")
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(listed, openapi);

    let sourced = OperationBuilder::get("/oagw/v1/plugins/{id}/source")
        .operation_id("oagw.get_plugin_source")
        .summary("Read a plugin's source")
        .description("The source of a tenant-defined plugin, verbatim")
        .tag(TAG_MANAGEMENT)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin identifier")
        .handler(handlers::plugins::get_plugin_source)
        .text_response(http::StatusCode::OK, "The plugin's source", "text/plain")
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(read, openapi);

    let replaced = OperationBuilder::put("/oagw/v1/plugins/{id}")
        .operation_id("oagw.replace_plugin")
        .summary("Replace a plugin")
        .description("Refused: a plugin is immutable, so a change is a new one")
        .tag(TAG_MANAGEMENT)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin identifier")
        .handler(handlers::plugins::replace_plugin)
        .text_response(
            http::StatusCode::BAD_REQUEST,
            "A plugin is immutable",
            "text/plain",
        )
        .error_400(openapi)
        .error_404(openapi)
        .register(sourced, openapi);

    OperationBuilder::delete("/oagw/v1/plugins/{id}")
        .operation_id("oagw.release_plugin")
        .summary("Release a plugin")
        .description("Refuse while a route binds the plugin, succeed once none does")
        .tag(TAG_MANAGEMENT)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin identifier")
        .handler(handlers::plugins::delete_plugin)
        .json_response(http::StatusCode::NO_CONTENT, "No route binds the plugin")
        .error_401(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(replaced, openapi)
}

fn register_proxy_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    for method in &PROXY_METHODS {
        openapi.register_operation(&proxy_spec(method, PROXY_PATH_WILDCARD));
        openapi.register_operation(&proxy_spec(method, PROXY_ALIAS_PATH));
    }

    let proxied = router.route(
        PROXY_PATH_WILDCARD,
        axum::routing::any(handlers::proxy::proxy_path),
    );
    proxied.route(
        PROXY_ALIAS_PATH,
        axum::routing::any(handlers::proxy::proxy_root),
    )
}

/// The `OpenAPI` operation describing one proxied method.
///
/// The gateway keys a handler by identifier, so every (verb, path) pair carries
/// its own.
fn proxy_spec(method: &http::Method, path: &str) -> OperationSpec {
    let verb = method.as_str().to_ascii_lowercase();
    // The two paths differ only in whether a sub-path is present, so the
    // identifier says which one the operation describes.
    let segment = if path.ends_with("/{*path}") {
        "subpath"
    } else {
        "root"
    };
    let handler_id = format!("oagw.proxy.{verb}.{segment}");
    OperationSpec {
        method: method.clone(),
        path: path.to_owned(),
        operation_id: Some(handler_id.clone()),
        summary: Some("Proxy an exchange".to_owned()),
        description: Some(
            "Forward the exchange to the upstream the alias names, streaming the request body \
             and the response alike; a WebSocket upgrade is bridged once the upstream agrees."
                .to_owned(),
        ),
        tags: vec![TAG_PROXY.to_owned()],
        params: vec![
            ParamSpec {
                name: "alias".to_owned(),
                location: ParamLocation::Path,
                required: true,
                description: Some("Upstream alias".to_owned()),
                param_type: "string".to_owned(),
                array: false,
            },
            ParamSpec {
                name: "path".to_owned(),
                location: ParamLocation::Path,
                required: false,
                description: Some("Remainder of the path forwarded to the upstream".to_owned()),
                param_type: "string".to_owned(),
                array: false,
            },
        ],
        request_body: None,
        responses: vec![ResponseSpec {
            status: http::StatusCode::OK.as_u16(),
            content_type: "application/json",
            description: "The upstream's response, streamed through".to_owned(),
            schema: None,
        }],
        handler_id,
        authenticated: true,
        exposed: false,
        rate_limit: None,
        allowed_request_content_types: None,
        vendor_extensions: VendorExtensions::default(),
        license_requirement: None,
    }
}
