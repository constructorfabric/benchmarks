//! Route registration.
//!
//! Paths are **gear-relative**: the api-gateway nests this router under its own
//! `prefix_path`, so repeating a prefix here would double it.

use std::sync::Arc;

use axum::Router;
use axum::routing::any;
use http::StatusCode;
use toolkit::api::{OpenApiRegistry, OperationBuilder, ParamLocation, ParamSpec};

use crate::domain::input::{PluginInput, RouteInput, UpstreamInput};

use super::dto::{ListResponse, PluginResponse, PluginSourceResponse};
use super::handlers;
use super::proxy;
use super::state::OagwState;

const TAG: &str = "Outbound API Gateway";

/// Base path of the management and proxy APIs, relative to the gateway prefix.
pub const BASE: &str = "/oagw/v1";

/// Describe the optional list query parameters once.
fn list_params<H, R, S, A, L>(
    builder: OperationBuilder<H, R, S, A, L>,
) -> OperationBuilder<H, R, S, A, L>
where
    H: toolkit::api::operation_builder::HandlerSlot<S>,
    A: toolkit::api::operation_builder::AuthState,
    L: toolkit::api::operation_builder::LicenseState,
{
    builder
        .param(query_param("$filter", "OData filter expression"))
        .param(query_param("$select", "Comma-separated fields to return"))
        .param(query_param("$orderby", "Sort order, e.g. `alias desc`"))
        .param(query_param("$top", "Maximum number of results"))
        .param(query_param("$skip", "Number of results to skip"))
}

fn query_param(name: &str, description: &str) -> ParamSpec {
    ParamSpec {
        name: name.to_owned(),
        location: ParamLocation::Query,
        required: false,
        description: Some(description.to_owned()),
        param_type: "string".to_owned(),
        array: false,
    }
}

fn target_host_param() -> ParamSpec {
    ParamSpec {
        name: "X-OAGW-Target-Host".to_owned(),
        location: ParamLocation::Header,
        required: false,
        description: Some(
            "Pins the request to one endpoint of a multi-endpoint upstream. Required when the \
             alias was derived from a common domain suffix."
                .to_owned(),
        ),
        param_type: "string".to_owned(),
        array: false,
    }
}

/// Register the management and proxy routes.
#[allow(clippy::too_many_lines, reason = "one linear table of route declarations")]
pub fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    state: Arc<OagwState>,
) -> Router {
    let router = register_upstreams(router, openapi);
    let router = register_routes_api(router, openapi);
    let router = register_plugins(router, openapi);
    let router = register_proxy(router, openapi);
    router.layer(axum::Extension(state))
}

fn register_upstreams(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::post(format!("{BASE}/upstreams"))
        .operation_id("oagw.create_upstream")
        .summary("Create an upstream")
        .description(
            "Register an external service. The alias is auto-derived from hostname endpoints; \
             IP-based or otherwise non-derivable pools must supply one explicitly.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<UpstreamInput>(openapi, "Upstream configuration")
        .handler(handlers::create_upstream)
        .json_response(StatusCode::CREATED, "Upstream created")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = list_params(
        OperationBuilder::get(format!("{BASE}/upstreams"))
            .operation_id("oagw.list_upstreams")
            .summary("List upstreams")
            .description("List the calling tenant's upstreams. Ancestor upstreams are not listed.")
            .tag(TAG),
    )
    .authenticated()
    .no_license_required()
    .handler(handlers::list_upstreams)
    .json_response_with_schema::<ListResponse>(openapi, StatusCode::OK, "Matching upstreams")
    .error_400(openapi)
    .error_401(openapi)
    .error_403(openapi)
    .error_500(openapi)
    .register(router, openapi);

    let router = OperationBuilder::get(format!("{BASE}/upstreams/{{id}}"))
        .operation_id("oagw.get_upstream")
        .summary("Get an upstream")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream UUID or `gts.cf.core.oagw.upstream.v1~{uuid}`")
        .handler(handlers::get_upstream)
        .json_response(StatusCode::OK, "The upstream")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::put(format!("{BASE}/upstreams/{{id}}"))
        .operation_id("oagw.replace_upstream")
        .summary("Replace an upstream")
        .description(
            "Full replacement: omitted optional fields are cleared. The alias is immutable — an \
             endpoint change that would move it is rejected.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream UUID or `gts.cf.core.oagw.upstream.v1~{uuid}`")
        .json_request::<UpstreamInput>(openapi, "Replacement upstream configuration")
        .handler(handlers::replace_upstream)
        .json_response(StatusCode::OK, "The replaced upstream")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    OperationBuilder::delete(format!("{BASE}/upstreams/{{id}}"))
        .operation_id("oagw.delete_upstream")
        .summary("Delete an upstream")
        .description("Deletes the upstream and cascades to its routes.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream UUID or `gts.cf.core.oagw.upstream.v1~{uuid}`")
        .handler(handlers::delete_upstream)
        .no_content_response(StatusCode::NO_CONTENT, "Upstream deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi)
}

fn register_routes_api(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::post(format!("{BASE}/routes"))
        .operation_id("oagw.create_route")
        .summary("Create a route")
        .description("Bind a match rule to one of the calling tenant's upstreams.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<RouteInput>(openapi, "Route configuration")
        .handler(handlers::create_route)
        .json_response(StatusCode::CREATED, "Route created")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = list_params(
        OperationBuilder::get(format!("{BASE}/routes"))
            .operation_id("oagw.list_routes")
            .summary("List routes")
            .tag(TAG),
    )
    .authenticated()
    .no_license_required()
    .handler(handlers::list_routes)
    .json_response_with_schema::<ListResponse>(openapi, StatusCode::OK, "Matching routes")
    .error_400(openapi)
    .error_401(openapi)
    .error_403(openapi)
    .error_500(openapi)
    .register(router, openapi);

    let router = OperationBuilder::get(format!("{BASE}/routes/{{id}}"))
        .operation_id("oagw.get_route")
        .summary("Get a route")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route UUID or `gts.cf.core.oagw.route.v1~{uuid}`")
        .handler(handlers::get_route)
        .json_response(StatusCode::OK, "The route")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::put(format!("{BASE}/routes/{{id}}"))
        .operation_id("oagw.replace_route")
        .summary("Replace a route")
        .description("Full replacement. `upstream_id` is immutable.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route UUID or `gts.cf.core.oagw.route.v1~{uuid}`")
        .json_request::<RouteInput>(openapi, "Replacement route configuration")
        .handler(handlers::replace_route)
        .json_response(StatusCode::OK, "The replaced route")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    OperationBuilder::delete(format!("{BASE}/routes/{{id}}"))
        .operation_id("oagw.delete_route")
        .summary("Delete a route")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route UUID or `gts.cf.core.oagw.route.v1~{uuid}`")
        .handler(handlers::delete_route)
        .no_content_response(StatusCode::NO_CONTENT, "Route deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi)
}

fn register_plugins(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::post(format!("{BASE}/plugins"))
        .operation_id("oagw.create_plugin")
        .summary("Create a custom plugin")
        .description("Plugin definitions are immutable; publish a new one to change behaviour.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<PluginInput>(openapi, "Plugin definition")
        .handler(handlers::create_plugin)
        .json_response_with_schema::<PluginResponse>(
            openapi,
            StatusCode::CREATED,
            "Plugin created",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = list_params(
        OperationBuilder::get(format!("{BASE}/plugins"))
            .operation_id("oagw.list_plugins")
            .summary("List custom plugins")
            .tag(TAG),
    )
    .authenticated()
    .no_license_required()
    .handler(handlers::list_plugins)
    .json_response_with_schema::<ListResponse>(openapi, StatusCode::OK, "Matching plugins")
    .error_400(openapi)
    .error_401(openapi)
    .error_403(openapi)
    .error_500(openapi)
    .register(router, openapi);

    let router = OperationBuilder::get(format!("{BASE}/plugins/{{id}}"))
        .operation_id("oagw.get_plugin")
        .summary("Get a custom plugin")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param(
            "id",
            "Plugin UUID or `gts.cf.core.oagw.{type}_plugin.v1~{uuid}`",
        )
        .handler(handlers::get_plugin)
        .json_response_with_schema::<PluginResponse>(openapi, StatusCode::OK, "The plugin")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(format!("{BASE}/plugins/{{id}}/source"))
        .operation_id("oagw.get_plugin_source")
        .summary("Get a custom plugin's source")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param(
            "id",
            "Plugin UUID or `gts.cf.core.oagw.{type}_plugin.v1~{uuid}`",
        )
        .handler(handlers::get_plugin_source)
        .json_response_with_schema::<PluginSourceResponse>(
            openapi,
            StatusCode::OK,
            "The plugin source",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    OperationBuilder::delete(format!("{BASE}/plugins/{{id}}"))
        .operation_id("oagw.delete_plugin")
        .summary("Delete a custom plugin")
        .description("Fails with 409 while any upstream or route still references the plugin.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param(
            "id",
            "Plugin UUID or `gts.cf.core.oagw.{type}_plugin.v1~{uuid}`",
        )
        .handler(handlers::delete_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "Plugin deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi)
}

fn register_proxy(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    // `any` rather than a fixed verb: the proxy relays whatever method the
    // caller used, including the `GET` that opens a WebSocket and the
    // `OPTIONS` of a CORS preflight.
    let router = OperationBuilder::post(format!("{BASE}/proxy/{{alias}}"))
        .operation_id("oagw.proxy_root")
        .summary("Proxy a request to an upstream")
        .description(
            "Resolves the alias across the tenant hierarchy, applies the effective \
             configuration, runs the plugin chain and forwards the call. Any HTTP method is \
             accepted; SSE responses and protocol upgrades are streamed.",
        )
        .tag(TAG)
        .param(target_host_param())
        .authenticated()
        .no_license_required()
        .method_router(any(proxy::proxy))
        .json_response(StatusCode::OK, "The upstream response, relayed")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_429(openapi)
        .error_500(openapi)
        .error_502(openapi)
        .error_503(openapi)
        .error_504(openapi)
        .register(router, openapi);

    OperationBuilder::post(format!("{BASE}/proxy/{{alias}}/{{*path}}"))
        .operation_id("oagw.proxy")
        .summary("Proxy a request to an upstream path")
        .description(
            "As `oagw.proxy_root`, with the path suffix appended to the matched route path when \
             `path_suffix_mode` is `append`.",
        )
        .tag(TAG)
        .param(target_host_param())
        .authenticated()
        .no_license_required()
        .method_router(any(proxy::proxy))
        .json_response(StatusCode::OK, "The upstream response, relayed")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_429(openapi)
        .error_500(openapi)
        .error_502(openapi)
        .error_503(openapi)
        .error_504(openapi)
        .register(router, openapi)
}
