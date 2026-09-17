//! REST route registration for the OAGW management API and the proxy data
//! plane.
//!
//! Paths are gear-relative (`/oagw/v1/...`); the api-gateway `prefix_path` for
//! this gear is empty, so these are the paths the graded configuration sees.
//!
//! The data plane exposes exactly two routes, `/oagw/v1/proxy/{alias}` and
//! `/oagw/v1/proxy/{alias}/{*path}`, under **every** HTTP method: the method is
//! a route-matching input, not an operation id. HEAD and OPTIONS are attached
//! with an explicit [`MethodRouter`], because [`OperationBuilder::handler`]
//! only maps the five body-carrying methods.

use std::sync::Arc;

use axum::routing::MethodRouter;
use axum::routing::{delete, get, head, options, patch, post, put};
use axum::{Extension, Router};
use http::Method;
use toolkit::api::OpenApiRegistry;
use toolkit::api::canonical_prelude::StatusCode;
use toolkit::api::operation_builder::{
    CORE_GLOBAL_BASE_LICENSE_FEATURE, LicenseFeature, OperationBuilder,
};

use super::handlers;
use super::handlers::SharedDataPlane;
use crate::domain::services::ManagementService;

const API_TAG: &str = "OAGW";

/// The alias path parameter, spelled in axum 0.8 syntax.
const ALIAS_PARAM: &str = "{alias}";
/// The catch-all path-suffix parameter, spelled in axum 0.8 syntax.
const PATH_PARAM: &str = "{*path}";

struct License;

impl AsRef<str> for License {
    fn as_ref(&self) -> &'static str {
        CORE_GLOBAL_BASE_LICENSE_FEATURE
    }
}

impl LicenseFeature for License {}

/// Registers all management routes and the two proxy routes for the OAGW gear.
///
/// The [`ManagementService`] and the [`SharedDataPlane`] are installed as
/// `axum::Extension`s, so every handler can extract them (and the handlers are
/// the only places that do).
#[allow(clippy::needless_pass_by_value)]
pub fn register_routes(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
    service: Arc<ManagementService>,
    data_plane: SharedDataPlane,
) -> Router {
    // POST /oagw/v1/upstreams
    router = OperationBuilder::post("/oagw/v1/upstreams")
        .operation_id("oagw.upstreams.create")
        .summary("Create upstream")
        .description(
            "Create an upstream. The alias is derived from the endpoint hostnames unless an \
             explicit alias is supplied (required for IP-based endpoints). Unique per tenant.",
        )
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .json_request::<crate::api::rest::dto::UpstreamRequestDto>(openapi, "Upstream to create")
        .handler(handlers::upstreams::create_upstream)
        .json_response_with_schema::<crate::api::rest::dto::UpstreamResponseDto>(
            openapi,
            StatusCode::CREATED,
            "The created upstream",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // GET /oagw/v1/upstreams
    router = OperationBuilder::get("/oagw/v1/upstreams")
        .operation_id("oagw.upstreams.list")
        .summary("List upstreams")
        .description("List the calling tenant's upstreams with OData `$filter`, `$select`, `$orderby`, `$top` and `$skip`.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .query_param("$filter", false, "OData filter expression (e.g. `alias eq 'api.openai.com'`)")
        .query_param("$select", false, "Comma-separated fields to return")
        .query_param("$orderby", false, "Sort order (e.g. `alias desc`)")
        .query_param("$top", false, "Max results (default 50, max 100)")
        .query_param("$skip", false, "Offset for pagination")
        .handler(handlers::upstreams::list_upstreams)
        .json_response_with_schema::<crate::api::rest::dto::UpstreamListDto>(
            openapi,
            StatusCode::OK,
            "The requested page of upstreams",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // GET /oagw/v1/upstreams/{id}
    router = OperationBuilder::get("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.upstreams.get")
        .summary("Get upstream")
        .description("Retrieve one upstream of the calling tenant by id.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Upstream id (UUID or GTS instance id)")
        .handler(handlers::upstreams::get_upstream)
        .json_response_with_schema::<crate::api::rest::dto::UpstreamResponseDto>(
            openapi,
            StatusCode::OK,
            "The requested upstream",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // PUT /oagw/v1/upstreams/{id}
    router = OperationBuilder::put("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.upstreams.replace")
        .summary("Replace upstream")
        .description(
            "Full replacement of an upstream: every field is overwritten and omitted optional \
             fields are cleared. The alias is immutable.",
        )
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Upstream id (UUID or GTS instance id)")
        .json_request::<crate::api::rest::dto::UpstreamRequestDto>(openapi, "Replacement upstream")
        .handler(handlers::upstreams::replace_upstream)
        .json_response_with_schema::<crate::api::rest::dto::UpstreamResponseDto>(
            openapi,
            StatusCode::OK,
            "The replaced upstream",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // DELETE /oagw/v1/upstreams/{id}
    router = OperationBuilder::delete("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.upstreams.delete")
        .summary("Delete upstream")
        .description("Delete an upstream. Routes that reference it are deleted as well.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Upstream id (UUID or GTS instance id)")
        .handler(handlers::upstreams::delete_upstream)
        .no_content_response(StatusCode::NO_CONTENT, "The upstream was deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // POST /oagw/v1/routes
    router = OperationBuilder::post("/oagw/v1/routes")
        .operation_id("oagw.routes.create")
        .summary("Create route")
        .description(
            "Create a route bound to an upstream of the calling tenant. The match rule must be \
             unique within the upstream.",
        )
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .json_request::<crate::api::rest::dto::RouteRequestDto>(openapi, "Route to create")
        .handler(handlers::routes::create_route)
        .json_response_with_schema::<crate::api::rest::dto::RouteResponseDto>(
            openapi,
            StatusCode::CREATED,
            "The created route",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // GET /oagw/v1/routes
    router = OperationBuilder::get("/oagw/v1/routes")
        .operation_id("oagw.routes.list")
        .summary("List routes")
        .description("List the calling tenant's routes with OData `$filter`, `$select`, `$orderby`, `$top` and `$skip`.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .query_param("$filter", false, "OData filter expression (e.g. `upstream_id eq '{uuid}'`)")
        .query_param("$select", false, "Comma-separated fields to return")
        .query_param("$orderby", false, "Sort order")
        .query_param("$top", false, "Max results (default 50, max 100)")
        .query_param("$skip", false, "Offset for pagination")
        .handler(handlers::routes::list_routes)
        .json_response_with_schema::<crate::api::rest::dto::RouteListDto>(
            openapi,
            StatusCode::OK,
            "The requested page of routes",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // GET /oagw/v1/routes/{id}
    router = OperationBuilder::get("/oagw/v1/routes/{id}")
        .operation_id("oagw.routes.get")
        .summary("Get route")
        .description("Retrieve one route of the calling tenant by id.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Route id (UUID or GTS instance id)")
        .handler(handlers::routes::get_route)
        .json_response_with_schema::<crate::api::rest::dto::RouteResponseDto>(
            openapi,
            StatusCode::OK,
            "The requested route",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // PUT /oagw/v1/routes/{id}
    router = OperationBuilder::put("/oagw/v1/routes/{id}")
        .operation_id("oagw.routes.replace")
        .summary("Replace route")
        .description(
            "Full replacement of a route. `upstream_id` is immutable: repeating the current value \
             is accepted, a different value is rejected.",
        )
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Route id (UUID or GTS instance id)")
        .json_request::<crate::api::rest::dto::RouteUpdateDto>(openapi, "Replacement route")
        .handler(handlers::routes::replace_route)
        .json_response_with_schema::<crate::api::rest::dto::RouteResponseDto>(
            openapi,
            StatusCode::OK,
            "The replaced route",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // DELETE /oagw/v1/routes/{id}
    router = OperationBuilder::delete("/oagw/v1/routes/{id}")
        .operation_id("oagw.routes.delete")
        .summary("Delete route")
        .description("Delete a route of the calling tenant.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Route id (UUID or GTS instance id)")
        .handler(handlers::routes::delete_route)
        .no_content_response(StatusCode::NO_CONTENT, "The route was deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // POST /oagw/v1/plugins
    router = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.plugins.create")
        .summary("Create plugin")
        .description(
            "Create a custom (UUID-backed) plugin. Plugins are immutable after creation, so there \
             is no replace operation.",
        )
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .json_request::<crate::api::rest::dto::PluginRequestDto>(openapi, "Plugin to create")
        .handler(handlers::plugins::create_plugin)
        .json_response_with_schema::<crate::api::rest::dto::PluginResponseDto>(
            openapi,
            StatusCode::CREATED,
            "The created plugin",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // GET /oagw/v1/plugins
    router = OperationBuilder::get("/oagw/v1/plugins")
        .operation_id("oagw.plugins.list")
        .summary("List plugins")
        .description("List the calling tenant's plugins with OData `$filter`, `$select`, `$orderby`, `$top` and `$skip`.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .query_param("$filter", false, "OData filter expression (e.g. `type eq 'guard'`)")
        .query_param("$select", false, "Comma-separated fields to return")
        .query_param("$orderby", false, "Sort order")
        .query_param("$top", false, "Max results (default 50, max 100)")
        .query_param("$skip", false, "Offset for pagination")
        .handler(handlers::plugins::list_plugins)
        .json_response_with_schema::<crate::api::rest::dto::PluginListDto>(
            openapi,
            StatusCode::OK,
            "The requested page of plugins",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // GET /oagw/v1/plugins/{id}
    router = OperationBuilder::get("/oagw/v1/plugins/{id}")
        .operation_id("oagw.plugins.get")
        .summary("Get plugin")
        .description("Retrieve one plugin of the calling tenant by id.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Plugin id (UUID or GTS instance id)")
        .handler(handlers::plugins::get_plugin)
        .json_response_with_schema::<crate::api::rest::dto::PluginResponseDto>(
            openapi,
            StatusCode::OK,
            "The requested plugin",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // GET /oagw/v1/plugins/{id}/source
    router = OperationBuilder::get("/oagw/v1/plugins/{id}/source")
        .operation_id("oagw.plugins.source")
        .summary("Get plugin source")
        .description("Retrieve the declared source / configuration content of a plugin.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Plugin id (UUID or GTS instance id)")
        .handler(handlers::plugins::get_plugin_source)
        .json_response_with_schema::<crate::api::rest::dto::PluginSourceResponseDto>(
            openapi,
            StatusCode::OK,
            "The declared plugin source",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // DELETE /oagw/v1/plugins/{id}
    router = OperationBuilder::delete("/oagw/v1/plugins/{id}")
        .operation_id("oagw.plugins.delete")
        .summary("Delete plugin")
        .description(
            "Delete a plugin. Returns `409` with the still-referencing upstreams and routes while \
             the plugin is bound.",
        )
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Plugin id (UUID or GTS instance id)")
        .handler(handlers::plugins::delete_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "The plugin was deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    register_proxy_routes(router, openapi, data_plane).layer(Extension(service))
}

/// Registers the two data-plane proxy routes under every HTTP method.
///
/// The `{*path}` suffix is a separate route, not an optional segment, so both
/// [`handlers::proxy::proxy_root`] and [`handlers::proxy::proxy_path`] are
/// registered. Every method a proxied exchange may use is bound: the method is
/// an input to route matching, not an operation id.
fn register_proxy_routes(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
    data_plane: SharedDataPlane,
) -> Router {
    for path in [
        format!("/oagw/v1/proxy/{ALIAS_PARAM}"),
        format!("/oagw/v1/proxy/{ALIAS_PARAM}/{PATH_PARAM}"),
    ] {
        router = register_proxy_method(
            router,
            openapi,
            Method::GET,
            &path,
            "Proxied upstream response (streamed; `text/event-stream` bodies are relayed chunk-by-chunk)",
        );
        router = register_proxy_method(
            router,
            openapi,
            Method::POST,
            &path,
            "Proxied upstream response; the request body is streamed through",
        );
        router = register_proxy_method(
            router,
            openapi,
            Method::PUT,
            &path,
            "Proxied upstream response; the request body is streamed through",
        );
        router = register_proxy_method(
            router,
            openapi,
            Method::PATCH,
            &path,
            "Proxied upstream response; the request body is streamed through",
        );
        router = register_proxy_method(
            router,
            openapi,
            Method::DELETE,
            &path,
            "Proxied upstream response",
        );
        router = register_proxy_method(
            router,
            openapi,
            Method::HEAD,
            &path,
            "Proxied upstream response headers without a body",
        );
        router = register_proxy_method(
            router,
            openapi,
            Method::OPTIONS,
            &path,
            "CORS preflight, or the upstream's own OPTIONS response",
        );
    }
    router.layer(Extension(data_plane))
}

/// Register one `(method, path)` proxy operation.
fn register_proxy_method(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    method: Method,
    path: &str,
    description: &str,
) -> Router {
    let catch_all = path.ends_with(PATH_PARAM);
    let builder = OperationBuilder::new(method.clone(), path)
        .operation_id(format!(
            "oagw.proxy.{}{}",
            method.as_str().to_ascii_lowercase(),
            if catch_all { "_path" } else { "" }
        ))
        .summary(format!("Proxy {} request", method.as_str()))
        .description(description.to_owned())
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("alias", "Upstream alias to proxy to");
    let builder = if catch_all {
        builder.path_param("path", "Path suffix forwarded to the upstream")
    } else {
        builder
    };
    builder
        .method_router(proxy_method_router(method, catch_all))
        .text_response(StatusCode::OK, description, "*/*")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_429(openapi)
        .error_500(openapi)
        .register(router, openapi)
}

/// The `MethodRouter` for one proxy `(method, path)` pair.
///
/// `catch_all` selects the handler that also extracts the `{*path}` suffix;
/// the two handlers differ only in that extractor.
fn proxy_method_router(method: Method, catch_all: bool) -> MethodRouter {
    match (method, catch_all) {
        (Method::GET, false) => get(handlers::proxy::proxy_root),
        (Method::GET, true) => get(handlers::proxy::proxy_path),
        (Method::POST, false) => post(handlers::proxy::proxy_root),
        (Method::POST, true) => post(handlers::proxy::proxy_path),
        (Method::PUT, false) => put(handlers::proxy::proxy_root),
        (Method::PUT, true) => put(handlers::proxy::proxy_path),
        (Method::PATCH, false) => patch(handlers::proxy::proxy_root),
        (Method::PATCH, true) => patch(handlers::proxy::proxy_path),
        (Method::DELETE, false) => delete(handlers::proxy::proxy_root),
        (Method::DELETE, true) => delete(handlers::proxy::proxy_path),
        (Method::HEAD, false) => head(handlers::proxy::proxy_root),
        (Method::HEAD, true) => head(handlers::proxy::proxy_path),
        (Method::OPTIONS, false) => options(handlers::proxy::proxy_root),
        (Method::OPTIONS, true) => options(handlers::proxy::proxy_path),
        _ => MethodRouter::new(),
    }
}
