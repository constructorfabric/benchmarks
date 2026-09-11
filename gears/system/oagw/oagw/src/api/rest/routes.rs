//! Route registration for the OAGW REST API (DESIGN §3.3).
//!
//! Paths are gear-relative — `/oagw/v1/...` — because the api-ingress gear
//! mounts every gear under its own `prefix_path`.

use std::sync::Arc;

use axum::Router;
use http::StatusCode;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationBuilder;

use crate::api::rest::handlers;
use crate::config::OagwConfig;
use crate::domain::services::management::ControlPlaneService;
use crate::infra::proxy::service::DataPlaneService;

const TAG: &str = "OAGW";
const UPSTREAMS: &str = "/oagw/v1/upstreams";
const UPSTREAM: &str = "/oagw/v1/upstreams/{id}";
const ROUTES: &str = "/oagw/v1/routes";
const ROUTE: &str = "/oagw/v1/routes/{id}";
const PLUGINS: &str = "/oagw/v1/plugins";
const PLUGIN: &str = "/oagw/v1/plugins/{id}";
const PLUGIN_SOURCE: &str = "/oagw/v1/plugins/{id}/source";
const PROXY: &str = "/oagw/v1/proxy/{*path}";

/// Wires every OAGW route into the supplied router.
pub fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    control_plane: Arc<ControlPlaneService>,
    data_plane: Arc<DataPlaneService>,
    config: OagwConfig,
) -> Router {
    macro_rules! crud {
        ($router:expr, $openapi:expr, $path:expr, $collection:expr, $id:expr, $create:expr, $list:expr, $get:expr, $put:expr, $delete:expr, $desc:expr) => {{
            let router = OperationBuilder::post($path)
                .operation_id(concat!("oagw.create_", $collection))
                .summary($desc)
                .tag(TAG)
                .authenticated()
                .no_license_required()
                .handler($create)
                .json_response(StatusCode::CREATED, $desc)
                .register($router, $openapi);
            let router = OperationBuilder::get($path)
                .operation_id(concat!("oagw.list_", $collection))
                .summary($desc)
                .tag(TAG)
                .authenticated()
                .no_license_required()
                .handler($list)
                .json_response(StatusCode::OK, $desc)
                .register(router, $openapi);
            let router = OperationBuilder::get($id)
                .operation_id(concat!("oagw.get_", $collection))
                .summary($desc)
                .tag(TAG)
                .authenticated()
                .no_license_required()
                .handler($get)
                .json_response(StatusCode::OK, $desc)
                .register(router, $openapi);
            let router = OperationBuilder::put($id)
                .operation_id(concat!("oagw.replace_", $collection))
                .summary($desc)
                .tag(TAG)
                .authenticated()
                .no_license_required()
                .handler($put)
                .json_response(StatusCode::OK, $desc)
                .register(router, $openapi);
            OperationBuilder::delete($id)
                .operation_id(concat!("oagw.delete_", $collection))
                .summary($desc)
                .tag(TAG)
                .authenticated()
                .no_license_required()
                .handler($delete)
                .no_content_response(StatusCode::NO_CONTENT, $desc)
                .register(router, $openapi)
        }};
    }

    let router = crud!(
        router,
        openapi,
        UPSTREAMS,
        "upstreams",
        UPSTREAM,
        handlers::management::create_upstream,
        handlers::management::list_upstreams,
        handlers::management::get_upstream,
        handlers::management::replace_upstream,
        handlers::management::delete_upstream,
        "Manage upstreams"
    );
    let router = crud!(
        router,
        openapi,
        ROUTES,
        "routes",
        ROUTE,
        handlers::management::create_route,
        handlers::management::list_routes,
        handlers::management::get_route,
        handlers::management::replace_route,
        handlers::management::delete_route,
        "Manage routes"
    );

    let router = OperationBuilder::post(PLUGINS)
        .operation_id("oagw.create_plugin")
        .summary("Create a custom plugin")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::management::create_plugin)
        .json_response(StatusCode::CREATED, "Create a plugin")
        .register(router, openapi);
    let router = OperationBuilder::get(PLUGINS)
        .operation_id("oagw.list_plugins")
        .summary("List plugins")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::management::list_plugins)
        .json_response(StatusCode::OK, "List plugins")
        .register(router, openapi);
    let router = OperationBuilder::get(PLUGIN)
        .operation_id("oagw.get_plugin")
        .summary("Get a plugin")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::management::get_plugin)
        .json_response(StatusCode::OK, "Get a plugin")
        .register(router, openapi);
    let router = OperationBuilder::get(PLUGIN_SOURCE)
        .operation_id("oagw.get_plugin_source")
        .summary("Get a plugin's Starlark source")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::management::get_plugin_source)
        .json_response(StatusCode::OK, "Plugin source")
        .register(router, openapi);
    let router = OperationBuilder::delete(PLUGIN)
        .operation_id("oagw.delete_plugin")
        .summary("Delete a custom plugin")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::management::delete_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "Delete a plugin")
        .register(router, openapi);

    with_extensions(
        proxy_operations(router, openapi),
        control_plane,
        data_plane,
        config,
    )
}

/// Attaches the gear's shared services as request extensions.
///
/// `Router::layer` only reaches routes registered *before* the call, so the
/// extensions are attached once, after every route exists — attaching them first
/// leaves each handler without the extension it extracts.
fn with_extensions(
    router: Router,
    control_plane: Arc<ControlPlaneService>,
    data_plane: Arc<DataPlaneService>,
    config: OagwConfig,
) -> Router {
    router
        .layer(axum::Extension(control_plane))
        .layer(axum::Extension(data_plane))
        .layer(axum::Extension(config))
}

/// Registers the proxy path for every proxied HTTP method.
///
/// One registration per method keeps the `OpenAPI` document honest while the
/// router still answers with a single handler.
fn proxy_operations(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    macro_rules! method {
        ($router:ident, $builder:ident, $id:expr, $desc:expr) => {
            OperationBuilder::$builder(PROXY)
                .operation_id(concat!("oagw.proxy_", stringify!($id)))
                .summary($desc)
                .description(concat!(
                    "Proxies the request to the upstream addressed by `{alias}`. ",
                    $desc
                ))
                .tag(TAG)
                .authenticated()
                .no_license_required()
                .handler(handlers::proxy::proxy)
                .json_response(StatusCode::OK, $desc)
                .register($router, openapi)
        };
    }
    let router = method!(router, get, get, "Proxy a GET request");
    let router = method!(router, post, post, "Proxy a POST request");
    let router = method!(router, put, put, "Proxy a PUT request");
    let router = method!(router, patch, patch, "Proxy a PATCH request");
    let router = method!(router, delete, delete, "Proxy a DELETE request");

    // A preflight is answered by the data plane itself (permissively, before
    // any upstream work) and browsers never attach credentials to it, so it is
    // registered anonymously rather than alongside the proxied methods.
    OperationBuilder::new(http::Method::OPTIONS, PROXY)
        .operation_id("oagw.proxy_options")
        .summary("Answer a CORS preflight")
        .description(
            "Answers a CORS preflight locally, without resolving an upstream \
             (ADR 0004).",
        )
        // `.anonymous()` decides both the auth and the license axes, so unlike
        // the proxied methods above there is no further licence call to make.
        .tag(TAG)
        .anonymous()
        // `OperationBuilder::handler` only maps the five proxied verbs and
        // would answer every other method with `405`, so the preflight is
        // mounted through the pre-composed method router instead.
        .method_router(axum::routing::options(handlers::proxy::proxy))
        .json_response(StatusCode::OK, "Answer a CORS preflight")
        .register(router, openapi)
}
