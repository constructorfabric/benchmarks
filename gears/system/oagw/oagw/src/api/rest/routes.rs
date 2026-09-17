//! REST route registration.
//!
//! All paths are **gear-relative**: `/oagw/v1/...` without an `/api` prefix.
//! The operator gateway in front of the gear adds `/api`.

use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use axum::Extension;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationBuilder;

use crate::api::rest::extractors::ApiState;
use crate::api::rest::handlers;

/// Base path of every OAGW operation.
pub const BASE: &str = "/oagw/v1";

/// Register every OAGW route on `router`.
#[allow(clippy::needless_pass_by_value)]
pub fn register_routes(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
    state: ApiState,
) -> Router {
    router = register_upstream_routes(router, openapi);
    router = register_route_routes(router, openapi);
    router = register_plugin_routes(router, openapi);
    router = register_proxy_routes(router, openapi);

    router.layer(Extension(state))
}

/// Publish a permissive JSON-object schema under a DTO name so that the
/// request-body `$ref` emitted by [`OperationBuilder::json_request_schema`]
/// resolves without dragging the whole domain model into the OpenAPI document.
fn declare_request_schema(openapi: &dyn OpenApiRegistry, name: &'static str) {
    let schema = utoipa::openapi::RefOr::T(utoipa::openapi::schema::Schema::Object(
        utoipa::openapi::schema::ObjectBuilder::new()
            .schema_type(utoipa::openapi::schema::SchemaType::AnyValue)
            .description(Some("Free-form JSON object; see the component schemas in `docs/schemas`."))
            .build(),
    ));
    openapi.ensure_schema_raw(name, vec![(name.to_owned(), schema)]);
}

fn register_upstream_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    declare_request_schema(openapi, "UpstreamRequestDto");

    let router = OperationBuilder::get(format!("{BASE}/upstreams"))
        .operation_id("oagw_list_upstreams")
        .tag("OAGW Upstreams")
        .authenticated()
        .no_license_required()
        .query_param("filter", false, "OData $filter (alias eq ...)")
        .query_param("search", false, "OData $search")
        .query_param("top", false, "OData $top")
        .query_param("skip", false, "OData $skip")
        .handler(handlers::upstreams::list)
        .json_response(StatusCode::OK, "Upstream collection")
        .error_401(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::post(format!("{BASE}/upstreams"))
        .operation_id("oagw_create_upstream")
        .tag("OAGW Upstreams")
        .authenticated()
        .no_license_required()
        .json_request_schema("UpstreamRequestDto", "Upstream configuration")
        .handler(handlers::upstreams::create)
        .json_response(StatusCode::CREATED, "Created upstream")
        .error_400(openapi)
        .error_401(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(format!("{BASE}/upstreams/{{id}}"))
        .operation_id("oagw_get_upstream")
        .tag("OAGW Upstreams")
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream id (UUID or GTS instance id)")
        .handler(handlers::upstreams::get)
        .json_response(StatusCode::OK, "Upstream")
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::put(format!("{BASE}/upstreams/{{id}}"))
        .operation_id("oagw_update_upstream")
        .tag("OAGW Upstreams")
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream id")
        .json_request_schema("UpstreamRequestDto", "Upstream configuration")
        .handler(handlers::upstreams::update)
        .json_response(StatusCode::OK, "Updated upstream")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::delete(format!("{BASE}/upstreams/{{id}}"))
        .operation_id("oagw_delete_upstream")
        .tag("OAGW Upstreams")
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream id")
        .handler(handlers::upstreams::delete)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .error_401(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    OperationBuilder::get(format!("{BASE}/upstreams/{{id}}/routes"))
        .operation_id("oagw_list_upstream_routes")
        .tag("OAGW Routes")
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream id")
        .handler(handlers::routes::list_for_upstream)
        .json_response(StatusCode::OK, "Routes of an upstream")
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi)
}

fn register_route_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    declare_request_schema(openapi, "RouteRequestDto");

    let router = OperationBuilder::get(format!("{BASE}/routes"))
        .operation_id("oagw_list_routes")
        .tag("OAGW Routes")
        .authenticated()
        .no_license_required()
        .query_param("filter", false, "OData $filter")
        .query_param("search", false, "OData $search")
        .query_param("top", false, "OData $top")
        .query_param("skip", false, "OData $skip")
        .handler(handlers::routes::list)
        .json_response(StatusCode::OK, "Route collection")
        .error_401(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::post(format!("{BASE}/routes"))
        .operation_id("oagw_create_route")
        .tag("OAGW Routes")
        .authenticated()
        .no_license_required()
        .json_request_schema("RouteRequestDto", "Route configuration")
        .handler(handlers::routes::create)
        .json_response(StatusCode::CREATED, "Created route")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(format!("{BASE}/routes/{{id}}"))
        .operation_id("oagw_get_route")
        .tag("OAGW Routes")
        .authenticated()
        .no_license_required()
        .path_param("id", "Route id")
        .handler(handlers::routes::get)
        .json_response(StatusCode::OK, "Route")
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::put(format!("{BASE}/routes/{{id}}"))
        .operation_id("oagw_update_route")
        .tag("OAGW Routes")
        .authenticated()
        .no_license_required()
        .path_param("id", "Route id")
        .json_request_schema("RouteRequestDto", "Route configuration")
        .handler(handlers::routes::update)
        .json_response(StatusCode::OK, "Updated route")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    OperationBuilder::delete(format!("{BASE}/routes/{{id}}"))
        .operation_id("oagw_delete_route")
        .tag("OAGW Routes")
        .authenticated()
        .no_license_required()
        .path_param("id", "Route id")
        .handler(handlers::routes::delete)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi)
}

fn register_plugin_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    declare_request_schema(openapi, "PluginRequestDto");

    let router = OperationBuilder::get(format!("{BASE}/plugins"))
        .operation_id("oagw_list_plugins")
        .tag("OAGW Plugins")
        .authenticated()
        .no_license_required()
        .query_param("search", false, "OData $search")
        .query_param("top", false, "OData $top")
        .query_param("skip", false, "OData $skip")
        .handler(handlers::plugins::list)
        .json_response(StatusCode::OK, "Plugin collection")
        .error_401(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(format!("{BASE}/plugins/catalog"))
        .operation_id("oagw_plugin_catalog")
        .tag("OAGW Plugins")
        .authenticated()
        .no_license_required()
        .handler(handlers::plugins::catalog)
        .json_response(StatusCode::OK, "Built-in plugin catalog")
        .error_401(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::post(format!("{BASE}/plugins"))
        .operation_id("oagw_create_plugin")
        .tag("OAGW Plugins")
        .authenticated()
        .no_license_required()
        .json_request_schema("PluginRequestDto", "Plugin definition")
        .handler(handlers::plugins::create)
        .json_response(StatusCode::CREATED, "Created plugin")
        .error_400(openapi)
        .error_401(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(format!("{BASE}/plugins/{{id}}"))
        .operation_id("oagw_get_plugin")
        .tag("OAGW Plugins")
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin id")
        .handler(handlers::plugins::get)
        .json_response(StatusCode::OK, "Plugin")
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(format!("{BASE}/plugins/{{id}}/source"))
        .operation_id("oagw_get_plugin_source")
        .tag("OAGW Plugins")
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin id")
        .handler(handlers::plugins::source)
        .json_response(StatusCode::OK, "Stored plugin source")
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // A plugin definition is immutable after creation (PRD
    // `cpt-cf-oagw-fr-plugin-system`, ADR-0002): there is deliberately no
    // `PUT /plugins/{id}` — a correction means creating a new plugin and
    // re-binding it.
    OperationBuilder::delete(format!("{BASE}/plugins/{{id}}"))
        .operation_id("oagw_delete_plugin")
        .tag("OAGW Plugins")
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin id")
        .handler(handlers::plugins::delete)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .error_401(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi)
}

fn register_proxy_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::get(format!("{BASE}/proxy/{{alias}}"))
        .operation_id("oagw_proxy_root")
        .tag("OAGW Proxy")
        .authenticated()
        .no_license_required()
        .path_param("alias", "Upstream routing alias")
        .method_router(axum::routing::any(handlers::proxy::handle_root))
        .json_response(StatusCode::OK, "Proxied response")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .problem_response(openapi, http::StatusCode::PAYLOAD_TOO_LARGE, "Payload Too Large")
        .error_429(openapi)
        .error_500(openapi)
        .error_502(openapi)
        .error_503(openapi)
        .error_504(openapi)
        .register(router, openapi);

    OperationBuilder::get(format!("{BASE}/proxy/{{alias}}/{{*path}}"))
        .operation_id("oagw_proxy_path")
        .tag("OAGW Proxy")
        .authenticated()
        .no_license_required()
        .path_param("alias", "Upstream routing alias")
        .path_param("path", "Target path suffix")
        .method_router(axum::routing::any(handlers::proxy::handle_suffix))
        .json_response(StatusCode::OK, "Proxied response")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .problem_response(openapi, http::StatusCode::PAYLOAD_TOO_LARGE, "Payload Too Large")
        .error_429(openapi)
        .error_500(openapi)
        .error_502(openapi)
        .error_503(openapi)
        .error_504(openapi)
        .register(router, openapi)
}

/// Convenience wrapper used by the gear declaration.
#[must_use]
pub fn build_router(
    state: ApiState,
) -> Arc<dyn Fn(Router, &dyn OpenApiRegistry) -> Router + Send + Sync> {
    Arc::new(move |router, openapi| register_routes(router, openapi, state.clone()))
}
