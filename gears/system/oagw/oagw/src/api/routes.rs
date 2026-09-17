//! REST route registration (`/oagw/v1/...`).

use std::sync::Arc;

use axum::Router;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::{OperationBuilder, normalize_to_axum_path, state};

use super::handlers;
use crate::domain::service::{ControlPlaneService, DataPlaneService};

/// Registers every OAGW REST route and attaches both services to the router.
pub fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    control: Arc<ControlPlaneService>,
    data: Arc<DataPlaneService>,
) -> Router {
    let router = register_control_plane(router, openapi);
    let router = register_data_plane(router, openapi);
    router
        .layer(axum::Extension(control))
        .layer(axum::Extension(data))
}

/// Adds the list query parameters `DESIGN.md` §"List Query Parameters"
/// documents for every OAGW collection endpoint.
///
/// A macro because the operation builder is a type-state chain: the parameters
/// are the same on every list operation, but its generic parameters are not.
macro_rules! list_query_params {
    ($builder:expr) => {
        $builder
            .query_param(
                "$filter",
                false,
                "OData-style filter: a single `field eq value` comparison against a top-level scalar field",
            )
            .query_param(
                "$select",
                false,
                "Comma-separated top-level fields to project each resource down to",
            )
            .query_param(
                "$orderby",
                false,
                "Top-level scalar field to sort by, optionally followed by 'asc' or 'desc'",
            )
            .query_param_typed(
                "$top",
                false,
                "Maximum number of results (default 50, max 100)",
                "integer",
            )
            .query_param_typed(
                "$skip",
                false,
                "Number of results to skip before the page is cut",
                "integer",
            )
    };
}

fn register_control_plane(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    // The handlers reach the services through router extensions; the services
    // themselves are attached once, by `register_routes`, to the finished
    // router.
    let router = list_query_params!(
        OperationBuilder::get("/oagw/v1/upstreams")
            .operation_id("oagw.list_upstreams")
            .summary("List upstreams")
            .description("List every upstream registered by the calling tenant.")
            .tag(OAGW_TAG)
    )
    .authenticated()
    .no_license_required()
    .handler(handlers::list_upstreams)
    .json_response(axum::http::StatusCode::OK, "Upstream collection")
    .standard_errors(openapi)
    .register(router, openapi);

    let router = OperationBuilder::post("/oagw/v1/upstreams")
        .operation_id("oagw.create_upstream")
        .summary("Create an upstream")
        .description("Register a new upstream and its endpoint pool.")
        .tag(OAGW_TAG)
        .authenticated()
        .no_license_required()
        .json_request_schema("Upstream", "Upstream definition")
        .handler(handlers::create_upstream)
        .json_response(axum::http::StatusCode::CREATED, "Created upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.get_upstream")
        .summary("Get an upstream")
        .description("Fetch one upstream by identifier.")
        .tag(OAGW_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream identifier")
        .handler(handlers::get_upstream)
        .json_response(axum::http::StatusCode::OK, "The upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::put("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.replace_upstream")
        .summary("Replace an upstream")
        .description("Replace an upstream definition wholesale.")
        .tag(OAGW_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream identifier")
        .json_request_schema("Upstream", "Upstream definition")
        .handler(handlers::replace_upstream)
        .json_response(axum::http::StatusCode::OK, "Replaced upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::delete("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.delete_upstream")
        .summary("Delete an upstream")
        .description("Delete an upstream and every route belonging to it.")
        .tag(OAGW_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream identifier")
        .handler(handlers::delete_upstream)
        .no_content_response(axum::http::StatusCode::NO_CONTENT, "Deleted")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = list_query_params!(
        OperationBuilder::get("/oagw/v1/routes")
            .operation_id("oagw.list_routes")
            .summary("List routes")
            .description("List every route registered by the calling tenant.")
            .tag(OAGW_TAG)
    )
    .authenticated()
    .no_license_required()
    .handler(handlers::list_routes)
    .json_response(axum::http::StatusCode::OK, "Route collection")
    .standard_errors(openapi)
    .register(router, openapi);

    let router = OperationBuilder::post("/oagw/v1/routes")
        .operation_id("oagw.create_route")
        .summary("Create a route")
        .description("Register a route on an existing upstream.")
        .tag(OAGW_TAG)
        .authenticated()
        .no_license_required()
        .json_request_schema("Route", "Route definition")
        .handler(handlers::create_route)
        .json_response(axum::http::StatusCode::CREATED, "Created route")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/routes/{id}")
        .operation_id("oagw.get_route")
        .summary("Get a route")
        .description("Fetch one route by identifier.")
        .tag(OAGW_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route identifier")
        .handler(handlers::get_route)
        .json_response(axum::http::StatusCode::OK, "The route")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::put("/oagw/v1/routes/{id}")
        .operation_id("oagw.replace_route")
        .summary("Replace a route")
        .description("Replace a route definition wholesale.")
        .tag(OAGW_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route identifier")
        .json_request_schema("Route", "Route definition")
        .handler(handlers::replace_route)
        .json_response(axum::http::StatusCode::OK, "Replaced route")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::delete("/oagw/v1/routes/{id}")
        .operation_id("oagw.delete_route")
        .summary("Delete a route")
        .description("Delete one route.")
        .tag(OAGW_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route identifier")
        .handler(handlers::delete_route)
        .no_content_response(axum::http::StatusCode::NO_CONTENT, "Deleted")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = list_query_params!(
        OperationBuilder::get("/oagw/v1/plugins")
            .operation_id("oagw.list_plugins")
            .summary("List plugins")
            .description("List built-in and tenant-defined plugins.")
            .tag(OAGW_TAG)
    )
    .authenticated()
    .no_license_required()
    .handler(handlers::list_plugins)
    .json_response(axum::http::StatusCode::OK, "Plugin catalogue")
    .standard_errors(openapi)
    .register(router, openapi);

    let router = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.create_plugin")
        .summary("Create a plugin")
        .description("Register a tenant-defined custom plugin definition.")
        .tag(OAGW_TAG)
        .authenticated()
        .no_license_required()
        .json_request_schema("CustomPlugin", "Plugin definition")
        .handler(handlers::create_plugin)
        .json_response(axum::http::StatusCode::CREATED, "Created plugin")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/plugins/{id}/source")
        .operation_id("oagw.get_plugin_source")
        .summary("Get plugin source")
        .description("Fetch the stored Starlark source of a custom plugin.")
        .tag(OAGW_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin identifier")
        .handler(handlers::get_plugin_source)
        .json_response(axum::http::StatusCode::OK, "Plugin source")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/plugins/{id}")
        .operation_id("oagw.get_plugin")
        .summary("Get a plugin")
        .description("Fetch one plugin definition by identifier.")
        .tag(OAGW_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin identifier")
        .handler(handlers::get_plugin)
        .json_response(axum::http::StatusCode::OK, "The plugin")
        .standard_errors(openapi)
        .register(router, openapi);

    OperationBuilder::delete("/oagw/v1/plugins/{id}")
        .operation_id("oagw.delete_plugin")
        .summary("Delete a plugin")
        .description("Delete a custom plugin that no chain references.")
        .tag(OAGW_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin identifier")
        .handler(handlers::delete_plugin)
        .no_content_response(axum::http::StatusCode::NO_CONTENT, "Deleted")
        .standard_errors(openapi)
        .register(router, openapi)
}

fn register_data_plane(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    // The proxy surface is method-agnostic: the gateway must see every verb,
    // including `OPTIONS` (CORS preflight) and upgrade-capable requests.
    let router = OperationBuilder::get("/oagw/v1/proxy/{alias}/{*path}")
        .operation_id("oagw.proxy_path")
        .summary("Proxy a request to an upstream")
        .description("Route a request through the matching route and plugin chain to the upstream.")
        .tag(OAGW_TAG)
        .anonymous()
        .path_param("alias", "Upstream alias")
        .path_param("path", "Remaining request path")
        .method_router(axum::routing::any(handlers::proxy_path))
        .json_response(axum::http::StatusCode::OK, "Relayed response")
        .standard_errors(openapi)
        .register(router, openapi);

    // The gateway builds its authentication policy from one operation spec per
    // `(method, path)`, so the `any()` axum route is only anonymous for `GET`
    // unless every relayable verb is declared too. A verb left undeclared falls
    // through to `require_auth_by_default` and is refused `401` before the
    // request ever reaches the relay.
    declare_relayed_verbs(openapi);

    OperationBuilder::get("/oagw/v1/proxy/{alias}")
        .operation_id("oagw.proxy_alias")
        .summary("Proxy a request to an upstream root")
        .description("Proxy a request whose path terminates at the alias boundary.")
        .tag(OAGW_TAG)
        .anonymous()
        .path_param("alias", "Upstream alias")
        .method_router(axum::routing::any(handlers::proxy_alias))
        .json_response(axum::http::StatusCode::OK, "Relayed response")
        .standard_errors(openapi)
        .register(router, openapi)
}

/// Declares every relayable verb anonymous on the proxy paths.
///
/// The gateway builds its authentication policy from one operation spec per
/// `(method, path)`, so the `any()` axum routes above are only anonymous for
/// `GET` unless every relayable verb is declared too. A verb left undeclared
/// falls through to `require_auth_by_default` and is refused `401` before the
/// request ever reaches the relay, so the remaining verbs are registered into
/// the `OpenAPI` registry only — the axum router already answers every method.
fn declare_relayed_verbs(openapi: &dyn OpenApiRegistry) {
    for verb in [
        axum::http::Method::POST,
        axum::http::Method::PUT,
        axum::http::Method::DELETE,
        axum::http::Method::PATCH,
        axum::http::Method::HEAD,
        axum::http::Method::OPTIONS,
    ] {
        declare_relayed_verb(
            openapi,
            &verb,
            "/oagw/v1/proxy/{alias}/{*path}",
            "oagw.proxy_path",
        );
        declare_relayed_verb(openapi, &verb, "/oagw/v1/proxy/{alias}", "oagw.proxy_alias");
    }
}

/// Declares one relayed verb anonymous in the `OpenAPI` registry without adding a
/// second axum route for it.
///
/// The gateway resolves a request's authentication requirement from the
/// operation spec carrying that request's method, so a method-agnostic relay
/// route needs one spec per verb. Only `GET` carries the full operation above;
/// the rest share this declaration.
fn declare_relayed_verb(
    openapi: &dyn OpenApiRegistry,
    verb: &axum::http::Method,
    path: &str,
    operation_id: &str,
) {
    let builder = OperationBuilder::<state::Missing, state::Missing, ()>::new(
        verb.clone(),
        normalize_to_axum_path(path),
    )
    .operation_id(format!("{operation_id}:{}", verb.as_str().to_lowercase()))
    .summary("Proxy a request to an upstream")
    .description("Method-agnostic relay surface; see `oagw.proxy_path` for the full contract.")
    .tag(OAGW_TAG)
    .anonymous();
    openapi.register_operation(builder.spec());
}

/// `OpenAPI` tag for every OAGW operation.
const OAGW_TAG: &str = "OAGW";
