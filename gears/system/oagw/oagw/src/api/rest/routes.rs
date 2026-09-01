//! REST route registration for the OAGW gear (DESIGN §5).
//!
//! Control-plane CRUD for upstreams, routes and custom plugins is declared
//! with the type-safe [`OperationBuilder`]; the data-plane proxy entry point is
//! one wildcard `any` route on `/oagw/v1/proxy/{alias}/{*suffix}`.

use std::sync::Arc;

use axum::Router;
use axum::extract::Extension;
use axum::http::StatusCode;
use axum::routing::any;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::{
    CORE_GLOBAL_BASE_LICENSE_FEATURE, LicenseFeature, OperationBuilder,
};
use utoipa::openapi::RefOr;
use utoipa::openapi::schema::{ArrayBuilder, ObjectBuilder, Schema, SchemaType, Type};

use crate::domain::services::data_plane::DataPlaneService;
use crate::domain::services::management::ControlPlaneService;

use super::handlers;

const API_TAG: &str = "OAGW";

struct License;

impl AsRef<str> for License {
    fn as_ref(&self) -> &'static str {
        CORE_GLOBAL_BASE_LICENSE_FEATURE
    }
}

impl LicenseFeature for License {}

/// Register all OAGW REST routes onto `router`.
#[allow(clippy::needless_pass_by_value)]
pub fn register_routes(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
    control: Arc<ControlPlaneService>,
    data_plane: Arc<DataPlaneService>,
) -> Router {
    register_resource_schemas(openapi);

    let base = "/oagw/v1";

    router = register_upstream_routes(router, base, openapi);
    router = register_route_routes(router, base, openapi);
    router = register_plugin_routes(router, base, openapi);

    // Proxy (data plane) — every verb, with and without a path suffix.
    // matchit cannot capture a bare `{alias}` on the `{*suffix}` route, so the
    // alias-root and alias-slash forms are registered separately.
    let proxy = any(handlers::proxy::proxy_handler);
    router = router.route(
        &format!("{base}/proxy/{{alias}}/{{*suffix}}"),
        proxy.clone(),
    );
    router = router.route(&format!("{base}/proxy/{{alias}}"), proxy.clone());
    router = router.route(&format!("{base}/proxy/{{alias}}/"), proxy);

    router = router
        .layer(Extension(control))
        .layer(Extension(data_plane));

    router
}

/// Upstream CRUD endpoints.
#[allow(clippy::needless_pass_by_value)]
fn register_upstream_routes(
    mut router: Router,
    base: &str,
    openapi: &dyn OpenApiRegistry,
) -> Router {
    router = OperationBuilder::post(format!("{base}/upstreams"))
        .operation_id("oagw.create_upstream")
        .summary("Create an upstream")
        .description(
            "Create an upstream configuration. The routing alias is derived from the server \
             endpoints when not supplied explicitly.",
        )
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .json_request_schema("Upstream", "Upstream configuration")
        .handler(handlers::upstreams::create_upstream)
        .json_response(StatusCode::CREATED, "Upstream created")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get(format!("{base}/upstreams"))
        .operation_id("oagw.list_upstreams")
        .summary("List upstreams")
        .description("List upstreams with OData support ($filter, $select, $orderby, $top, $skip).")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .query_param("$filter", false, "OData filter expression")
        .query_param("$select", false, "OData select expression")
        .query_param("$orderby", false, "OData orderby expression")
        .query_param_typed("$top", false, "Maximum number of items", "integer")
        .query_param_typed("$skip", false, "Number of items to skip", "integer")
        .handler(handlers::upstreams::list_upstreams)
        .json_response(StatusCode::OK, "List of upstreams")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get(format!("{base}/upstreams/{{id}}"))
        .operation_id("oagw.get_upstream")
        .summary("Get an upstream")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Upstream UUID")
        .handler(handlers::upstreams::get_upstream)
        .json_response(StatusCode::OK, "The upstream")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Upstream not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::put(format!("{base}/upstreams/{{id}}"))
        .operation_id("oagw.update_upstream")
        .summary("Replace an upstream")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Upstream UUID")
        .json_request_schema("Upstream", "Replacement upstream configuration")
        .handler(handlers::upstreams::update_upstream)
        .json_response(StatusCode::OK, "Updated upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete(format!("{base}/upstreams/{{id}}"))
        .operation_id("oagw.delete_upstream")
        .summary("Delete an upstream")
        .description("Deletes the upstream and cascades to its routes.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Upstream UUID")
        .handler(handlers::upstreams::delete_upstream)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .standard_errors(openapi)
        .register(router, openapi);

    router
}

/// Route CRUD endpoints.
#[allow(clippy::needless_pass_by_value)]
fn register_route_routes(mut router: Router, base: &str, openapi: &dyn OpenApiRegistry) -> Router {
    router = OperationBuilder::post(format!("{base}/routes"))
        .operation_id("oagw.create_route")
        .summary("Create a route")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .json_request_schema("Route", "Route configuration")
        .handler(handlers::routes::create_route)
        .json_response(StatusCode::CREATED, "Route created")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get(format!("{base}/routes"))
        .operation_id("oagw.list_routes")
        .summary("List routes")
        .description("List routes with OData support; filter by `upstream_id eq '<uuid>'`.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .query_param("$filter", false, "OData filter expression")
        .query_param("$select", false, "OData select expression")
        .query_param("$orderby", false, "OData orderby expression")
        .query_param_typed("$top", false, "Maximum number of items", "integer")
        .query_param_typed("$skip", false, "Number of items to skip", "integer")
        .handler(handlers::routes::list_routes)
        .json_response(StatusCode::OK, "List of routes")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get(format!("{base}/routes/{{id}}"))
        .operation_id("oagw.get_route")
        .summary("Get a route")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Route UUID")
        .handler(handlers::routes::get_route)
        .json_response(StatusCode::OK, "The route")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Route not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::put(format!("{base}/routes/{{id}}"))
        .operation_id("oagw.update_route")
        .summary("Replace a route")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Route UUID")
        .json_request_schema("Route", "Replacement route configuration")
        .handler(handlers::routes::update_route)
        .json_response(StatusCode::OK, "Updated route")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete(format!("{base}/routes/{{id}}"))
        .operation_id("oagw.delete_route")
        .summary("Delete a route")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Route UUID")
        .handler(handlers::routes::delete_route)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .standard_errors(openapi)
        .register(router, openapi);

    router
}

/// Custom-plugin CRUD endpoints.
#[allow(clippy::needless_pass_by_value)]
fn register_plugin_routes(mut router: Router, base: &str, openapi: &dyn OpenApiRegistry) -> Router {
    router = OperationBuilder::post(format!("{base}/plugins"))
        .operation_id("oagw.create_plugin")
        .summary("Create a custom plugin")
        .description("Create a custom (UUID-backed) plugin. Immutable after creation.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .json_request_schema("Plugin", "Plugin definition")
        .handler(handlers::plugins::create_plugin)
        .json_response(StatusCode::CREATED, "Plugin created")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get(format!("{base}/plugins"))
        .operation_id("oagw.list_plugins")
        .summary("List plugins")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .query_param("$filter", false, "OData filter expression")
        .query_param("$select", false, "OData select expression")
        .query_param("$orderby", false, "OData orderby expression")
        .query_param_typed("$top", false, "Maximum number of items", "integer")
        .query_param_typed("$skip", false, "Number of items to skip", "integer")
        .handler(handlers::plugins::list_plugins)
        .json_response(StatusCode::OK, "List of plugins")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get(format!("{base}/plugins/{{id}}"))
        .operation_id("oagw.get_plugin")
        .summary("Get a plugin")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Plugin UUID")
        .handler(handlers::plugins::get_plugin)
        .json_response(StatusCode::OK, "The plugin")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Plugin not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get(format!("{base}/plugins/{{id}}/source"))
        .operation_id("oagw.get_plugin_source")
        .summary("Get a plugin's Starlark source")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Plugin UUID")
        .handler(handlers::plugins::get_plugin_source)
        .text_response(StatusCode::OK, "Plugin source code", "text/plain")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Plugin not found")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete(format!("{base}/plugins/{{id}}"))
        .operation_id("oagw.delete_plugin")
        .summary("Delete a plugin")
        .description("Deletes a custom plugin; 409 when it is referenced by a configuration.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Plugin UUID")
        .handler(handlers::plugins::delete_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .standard_errors(openapi)
        .register(router, openapi);

    router
}

/// Register the `Upstream`, `Route` and `Plugin` component schemas by name so
/// the name-based request-body `$ref`s never dangle in the `OpenAPI` document.
#[allow(clippy::too_many_lines)]
fn register_resource_schemas(openapi: &dyn OpenApiRegistry) {
    openapi.ensure_schema_raw(
        "Upstream",
        vec![(
            "Upstream".to_owned(),
            schema_object(vec![
                ("id", string()),
                ("tenant_id", string()),
                ("enabled", boolean()),
                ("alias", string()),
                ("tags", array(string())),
                ("server", schema_object(vec![])),
                ("protocol", string()),
                ("auth", schema_object(vec![])),
                ("headers", schema_object(vec![])),
                ("plugins", schema_object(vec![])),
                ("rate_limit", schema_object(vec![])),
                ("cors", schema_object(vec![])),
            ]),
        )],
    );
    openapi.ensure_schema_raw(
        "Route",
        vec![(
            "Route".to_owned(),
            schema_object(vec![
                ("id", string()),
                ("tenant_id", string()),
                ("tags", array(string())),
                ("upstream_id", string()),
                ("match", schema_object(vec![])),
                ("enabled", boolean()),
                ("plugins", schema_object(vec![])),
                ("rate_limit", schema_object(vec![])),
            ]),
        )],
    );
    openapi.ensure_schema_raw(
        "Plugin",
        vec![(
            "Plugin".to_owned(),
            schema_object(vec![
                ("id", string()),
                ("tenant_id", string()),
                ("plugin_type", string()),
                ("name", string()),
                ("description", string()),
                ("config_schema", schema_object(vec![])),
                ("source_code", string()),
                ("phases", array(string())),
            ]),
        )],
    );
}

/// A free-form object schema.
fn schema_object(props: Vec<(&str, RefOr<Schema>)>) -> RefOr<Schema> {
    let mut builder = ObjectBuilder::new().schema_type(SchemaType::Type(Type::Object));
    for (name, schema) in props {
        builder = builder.property(name, schema);
    }
    RefOr::T(Schema::Object(builder.build()))
}

/// A string schema.
fn string() -> RefOr<Schema> {
    RefOr::T(Schema::Object(
        ObjectBuilder::new()
            .schema_type(SchemaType::Type(Type::String))
            .build(),
    ))
}

/// A boolean schema.
fn boolean() -> RefOr<Schema> {
    RefOr::T(Schema::Object(
        ObjectBuilder::new()
            .schema_type(SchemaType::Type(Type::Boolean))
            .build(),
    ))
}

/// An array schema of `item`.
fn array(item: RefOr<Schema>) -> RefOr<Schema> {
    RefOr::T(Schema::Array(ArrayBuilder::new().items(item).build()))
}
