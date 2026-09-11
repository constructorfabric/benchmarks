//! Management REST handlers for upstreams, routes and plugins.
//!
//! Paths are registered gear-relative (`/oagw/v1/...`): the hosting gateway
//! adds its own prefix, so repeating it here would double it.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, RawQuery};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationBuilder;
use toolkit_security::SecurityContext;

use crate::api::error::OagwError;
use crate::api::rest::state::{ListQuery, OagwState, to_rows};
use crate::domain::model::{Plugin, PluginType, Route, Upstream};
use crate::domain::service::ControlPlaneService as _;
use crate::domain::service::control_plane_error;

/// Reads a `JSON` request body, mapping a malformed body onto the documented
/// `400` instead of the extractor's default `422`.
fn read_body<T>(
    body: Result<Json<T>, axum::extract::rejection::JsonRejection>,
) -> Result<T, OagwError> {
    body.map(|Json(value)| value).map_err(|rejection| {
        OagwError::validation(format!(
            "the request body does not satisfy the schema: {}",
            rejection.body_text()
        ))
    })
}

/// `OpenAPI` tag for the upstream catalogue.
const UPSTREAM_TAG: &str = "OAGW Upstreams";
/// `OpenAPI` tag for the route catalogue.
const ROUTE_TAG: &str = "OAGW Routes";
/// `OpenAPI` tag for the plugin catalogue.
const PLUGIN_TAG: &str = "OAGW Plugins";

/// Registers every management endpoint.
pub fn register(router: axum::Router, openapi: &dyn OpenApiRegistry) -> axum::Router {
    let router = register_upstreams(router, openapi);
    let router = register_routes_routes(router, openapi);
    register_plugins(router, openapi)
}

/// Request body of `POST /oagw/v1/plugins`.
#[derive(Debug)]
#[toolkit_macros::api_dto(request)]
pub struct CreatePluginRequest {
    /// Human readable plugin name.
    pub name: String,
    /// Which of the three plugin families this belongs to.
    pub plugin_type: PluginType,
    /// `Starlark` source.
    #[serde(default)]
    pub source_code: String,
}

/// Body of `GET /oagw/v1/plugins/{id}/source`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PluginSource {
    /// The plugin's identifier.
    pub id: String,
    /// The plugin's `Starlark` source.
    pub source_code: String,
}

// -------------------------------------------------------------------------------------------------
// Upstreams
// -------------------------------------------------------------------------------------------------

fn register_upstreams(mut router: axum::Router, openapi: &dyn OpenApiRegistry) -> axum::Router {
    router = OperationBuilder::post("/oagw/v1/upstreams")
        .operation_id("oagw.create_upstream")
        .summary("Create an upstream")
        .description("Registers an upstream service under a derived or explicit alias.")
        .tag(UPSTREAM_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<Upstream>(openapi, "Upstream definition")
        .handler(create_upstream)
        .json_response_with_schema::<Upstream>(openapi, StatusCode::CREATED, "Upstream created")
        .error_400(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams")
        .operation_id("oagw.list_upstreams")
        .summary("List upstreams")
        .description("Lists the calling tenant's upstreams with OData-style list parameters.")
        .tag(UPSTREAM_TAG)
        .authenticated()
        .no_license_required()
        .query_param_typed("$top", false, "Page size, at most 100", "integer")
        .query_param_typed("$skip", false, "Entries to skip", "integer")
        .query_param_typed("$orderby", false, "Field to order by", "string")
        .query_param_typed("$filter", false, "Field eq 'value' filter", "string")
        .query_param_typed("$select", false, "Comma separated fields to project", "string")
        .handler(list_upstreams)
        .json_array_response_with_schema::<Upstream>(openapi, StatusCode::OK, "Upstreams")
        .error_400(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.get_upstream")
        .summary("Read an upstream")
        .description("Reads one upstream owned by the calling tenant.")
        .tag(UPSTREAM_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream GTS identifier")
        .handler(get_upstream)
        .json_response_with_schema::<Upstream>(openapi, StatusCode::OK, "Upstream")
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.replace_upstream")
        .summary("Replace an upstream")
        .description("Replaces an upstream's configuration. The alias is immutable.")
        .tag(UPSTREAM_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream GTS identifier")
        .json_request::<Upstream>(openapi, "Upstream definition")
        .handler(replace_upstream)
        .json_response_with_schema::<Upstream>(openapi, StatusCode::OK, "Upstream replaced")
        .error_400(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    OperationBuilder::delete("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.delete_upstream")
        .summary("Delete an upstream")
        .description("Deletes an upstream together with the routes that reference it.")
        .tag(UPSTREAM_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream GTS identifier")
        .handler(delete_upstream)
        .no_content_response(StatusCode::NO_CONTENT, "Upstream deleted")
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi)
}

async fn create_upstream(
    axum::Extension(ctx): axum::Extension<SecurityContext>,
    axum::Extension(state): axum::Extension<Arc<OagwState>>,
    body: Result<Json<Upstream>, axum::extract::rejection::JsonRejection>,
) -> Result<(StatusCode, Json<Upstream>), OagwError> {
    let upstream = read_body(body)?;
    let created = state
        .control_plane
        .create_upstream(ctx.subject_tenant_id(), upstream)
        .await
        .map_err(control_plane_error)?;
    Ok((StatusCode::CREATED, Json(created)))
}

async fn list_upstreams(
    axum::Extension(ctx): axum::Extension<SecurityContext>,
    axum::Extension(state): axum::Extension<Arc<OagwState>>,
    RawQuery(query): RawQuery,
) -> Result<axum::response::Response, OagwError> {
    let items = state.control_plane.list_upstreams(ctx.subject_tenant_id()).await;
    Ok(list_response(&ListQuery::parse(query.as_deref().unwrap_or_default()), items))
}

async fn get_upstream(
    axum::Extension(ctx): axum::Extension<SecurityContext>,
    axum::Extension(state): axum::Extension<Arc<OagwState>>,
    Path(id): Path<String>,
) -> Result<Json<Upstream>, OagwError> {
    let found = state
        .control_plane
        .get_upstream(ctx.subject_tenant_id(), &id)
        .await
        .map_err(control_plane_error)?;
    Ok(Json(found))
}

async fn replace_upstream(
    axum::Extension(ctx): axum::Extension<SecurityContext>,
    axum::Extension(state): axum::Extension<Arc<OagwState>>,
    Path(id): Path<String>,
    body: Result<Json<Upstream>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Upstream>, OagwError> {
    let upstream = read_body(body)?;
    let replaced = state
        .control_plane
        .replace_upstream(ctx.subject_tenant_id(), &id, upstream)
        .await
        .map_err(control_plane_error)?;
    Ok(Json(replaced))
}

async fn delete_upstream(
    axum::Extension(ctx): axum::Extension<SecurityContext>,
    axum::Extension(state): axum::Extension<Arc<OagwState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, OagwError> {
    state
        .control_plane
        .delete_upstream(ctx.subject_tenant_id(), &id)
        .await
        .map_err(control_plane_error)?;
    Ok(StatusCode::NO_CONTENT)
}

// -------------------------------------------------------------------------------------------------
// Routes
// -------------------------------------------------------------------------------------------------

fn register_routes_routes(mut router: axum::Router, openapi: &dyn OpenApiRegistry) -> axum::Router {
    router = OperationBuilder::post("/oagw/v1/routes")
        .operation_id("oagw.create_route")
        .summary("Create a route")
        .description("Registers a match rule against an upstream owned by the calling tenant.")
        .tag(ROUTE_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<Route>(openapi, "Route definition")
        .handler(create_route)
        .json_response_with_schema::<Route>(openapi, StatusCode::CREATED, "Route created")
        .error_400(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes")
        .operation_id("oagw.list_routes")
        .summary("List routes")
        .description("Lists the calling tenant's routes with OData-style list parameters.")
        .tag(ROUTE_TAG)
        .authenticated()
        .no_license_required()
        .query_param_typed("$top", false, "Page size, at most 100", "integer")
        .query_param_typed("$skip", false, "Entries to skip", "integer")
        .query_param_typed("$orderby", false, "Field to order by", "string")
        .query_param_typed("$filter", false, "Field eq 'value' filter", "string")
        .query_param_typed("$select", false, "Comma separated fields to project", "string")
        .handler(list_routes)
        .json_array_response_with_schema::<Route>(openapi, StatusCode::OK, "Routes")
        .error_400(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes/{id}")
        .operation_id("oagw.get_route")
        .summary("Read a route")
        .description("Reads one route owned by the calling tenant.")
        .tag(ROUTE_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route GTS identifier")
        .handler(get_route)
        .json_response_with_schema::<Route>(openapi, StatusCode::OK, "Route")
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/routes/{id}")
        .operation_id("oagw.replace_route")
        .summary("Replace a route")
        .description("Replaces a route's match rules. The upstream reference is immutable.")
        .tag(ROUTE_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route GTS identifier")
        .json_request::<Route>(openapi, "Route definition")
        .handler(replace_route)
        .json_response_with_schema::<Route>(openapi, StatusCode::OK, "Route replaced")
        .error_400(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    OperationBuilder::delete("/oagw/v1/routes/{id}")
        .operation_id("oagw.delete_route")
        .summary("Delete a route")
        .description("Removes a route from the match table.")
        .tag(ROUTE_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route GTS identifier")
        .handler(delete_route)
        .no_content_response(StatusCode::NO_CONTENT, "Route deleted")
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi)
}

async fn create_route(
    axum::Extension(ctx): axum::Extension<SecurityContext>,
    axum::Extension(state): axum::Extension<Arc<OagwState>>,
    body: Result<Json<Route>, axum::extract::rejection::JsonRejection>,
) -> Result<(StatusCode, Json<Route>), OagwError> {
    let route = read_body(body)?;
    let created = state
        .control_plane
        .create_route(ctx.subject_tenant_id(), route)
        .await
        .map_err(control_plane_error)?;
    Ok((StatusCode::CREATED, Json(created)))
}

async fn list_routes(
    axum::Extension(ctx): axum::Extension<SecurityContext>,
    axum::Extension(state): axum::Extension<Arc<OagwState>>,
    RawQuery(query): RawQuery,
) -> Result<axum::response::Response, OagwError> {
    let items = state.control_plane.list_routes(ctx.subject_tenant_id()).await;
    Ok(list_response(&ListQuery::parse(query.as_deref().unwrap_or_default()), items))
}

async fn get_route(
    axum::Extension(ctx): axum::Extension<SecurityContext>,
    axum::Extension(state): axum::Extension<Arc<OagwState>>,
    Path(id): Path<String>,
) -> Result<Json<Route>, OagwError> {
    let found = state
        .control_plane
        .get_route(ctx.subject_tenant_id(), &id)
        .await
        .map_err(control_plane_error)?;
    Ok(Json(found))
}

async fn replace_route(
    axum::Extension(ctx): axum::Extension<SecurityContext>,
    axum::Extension(state): axum::Extension<Arc<OagwState>>,
    Path(id): Path<String>,
    body: Result<Json<Route>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Route>, OagwError> {
    let route = read_body(body)?;
    let replaced = state
        .control_plane
        .replace_route(ctx.subject_tenant_id(), &id, route)
        .await
        .map_err(control_plane_error)?;
    Ok(Json(replaced))
}

async fn delete_route(
    axum::Extension(ctx): axum::Extension<SecurityContext>,
    axum::Extension(state): axum::Extension<Arc<OagwState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, OagwError> {
    state
        .control_plane
        .delete_route(ctx.subject_tenant_id(), &id)
        .await
        .map_err(control_plane_error)?;
    Ok(StatusCode::NO_CONTENT)
}

// -------------------------------------------------------------------------------------------------
// Plugins
// -------------------------------------------------------------------------------------------------

fn register_plugins(mut router: axum::Router, openapi: &dyn OpenApiRegistry) -> axum::Router {
    router = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.create_plugin")
        .summary("Register a custom plugin")
        .description("Stores a Starlark plugin source. Plugins are immutable once stored.")
        .tag(PLUGIN_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<CreatePluginRequest>(openapi, "Plugin definition")
        .handler(create_plugin)
        .json_response_with_schema::<Plugin>(openapi, StatusCode::CREATED, "Plugin stored")
        .error_400(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins")
        .operation_id("oagw.list_plugins")
        .summary("List custom plugins")
        .description("Lists the calling tenant's custom plugins.")
        .tag(PLUGIN_TAG)
        .authenticated()
        .no_license_required()
        .query_param_typed("$top", false, "Page size, at most 100", "integer")
        .query_param_typed("$skip", false, "Entries to skip", "integer")
        .query_param_typed("$orderby", false, "Field to order by", "string")
        .query_param_typed("$filter", false, "Field eq 'value' filter", "string")
        .query_param_typed("$select", false, "Comma separated fields to project", "string")
        .handler(list_plugins)
        .json_array_response_with_schema::<Plugin>(openapi, StatusCode::OK, "Plugins")
        .error_400(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{id}")
        .operation_id("oagw.get_plugin")
        .summary("Read a custom plugin")
        .description("Reads one custom plugin owned by the calling tenant.")
        .tag(PLUGIN_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin GTS identifier")
        .handler(get_plugin)
        .json_response_with_schema::<Plugin>(openapi, StatusCode::OK, "Plugin")
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{id}/source")
        .operation_id("oagw.get_plugin_source")
        .summary("Read a plugin's source")
        .description("Returns the Starlark source of a custom plugin as plain text.")
        .tag(PLUGIN_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin GTS identifier")
        .handler(plugin_source)
        .text_response(StatusCode::OK, "Plugin source", "text/plain")
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/plugins/{id}")
        .operation_id("oagw.delete_plugin")
        .summary("Delete a custom plugin")
        .description("Deletes an unreferenced plugin; a referenced one is rejected with 409.")
        .tag(PLUGIN_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin GTS identifier")
        .handler(delete_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "Plugin deleted")
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // Plugins are immutable: the PUT is registered only to answer 405 with the
    // standard problem document.
    OperationBuilder::put("/oagw/v1/plugins/{id}")
        .operation_id("oagw.replace_plugin")
        .summary("Replace a plugin (unsupported)")
        .description("Plugins are immutable; this endpoint always answers 405.")
        .tag(PLUGIN_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin GTS identifier")
        .handler(replace_plugin)
        .json_response(StatusCode::METHOD_NOT_ALLOWED, "Plugins are immutable")
        .error_404(openapi)
        .register(router, openapi)
}

async fn create_plugin(
    axum::Extension(ctx): axum::Extension<SecurityContext>,
    axum::Extension(state): axum::Extension<Arc<OagwState>>,
    body: Result<Json<CreatePluginRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<(StatusCode, Json<Plugin>), OagwError> {
    let request = read_body(body)?;
    let created = state
        .control_plane
        .create_plugin(
            ctx.subject_tenant_id(),
            &request.name,
            request.plugin_type,
            &request.source_code,
        )
        .await
        .map_err(control_plane_error)?;
    Ok((StatusCode::CREATED, Json(created)))
}

async fn list_plugins(
    axum::Extension(ctx): axum::Extension<SecurityContext>,
    axum::Extension(state): axum::Extension<Arc<OagwState>>,
    RawQuery(query): RawQuery,
) -> Result<axum::response::Response, OagwError> {
    let items = state.control_plane.list_plugins(ctx.subject_tenant_id()).await;
    Ok(list_response(&ListQuery::parse(query.as_deref().unwrap_or_default()), items))
}

async fn get_plugin(
    axum::Extension(ctx): axum::Extension<SecurityContext>,
    axum::Extension(state): axum::Extension<Arc<OagwState>>,
    Path(id): Path<String>,
) -> Result<Json<Plugin>, OagwError> {
    let found = state
        .control_plane
        .get_plugin(ctx.subject_tenant_id(), &id)
        .await
        .map_err(control_plane_error)?;
    Ok(Json(found))
}

async fn plugin_source(
    axum::Extension(ctx): axum::Extension<SecurityContext>,
    axum::Extension(state): axum::Extension<Arc<OagwState>>,
    Path(id): Path<String>,
) -> Result<Response, OagwError> {
    let found = state
        .control_plane
        .get_plugin(ctx.subject_tenant_id(), &id)
        .await
        .map_err(control_plane_error)?;
    let body = axum::body::Body::from(found.source_code);
    let mut response = Response::new(body);
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    Ok(response)
}

async fn delete_plugin(
    axum::Extension(ctx): axum::Extension<SecurityContext>,
    axum::Extension(state): axum::Extension<Arc<OagwState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, OagwError> {
    state
        .control_plane
        .delete_plugin(ctx.subject_tenant_id(), &id)
        .await
        .map_err(control_plane_error)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Plugins are immutable: `PUT` is never accepted.
async fn replace_plugin(Path(id): Path<String>) -> OagwError {
    OagwError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        crate::domain::ids::ERR_PLUGIN_NOT_FOUND,
        "Method Not Allowed",
        format!("plugins are immutable; '{id}' cannot be replaced"),
    )
    .with_code("METHOD_NOT_ALLOWED")
}

// -------------------------------------------------------------------------------------------------
// Helpers
// -------------------------------------------------------------------------------------------------

/// Renders a list endpoint response honouring the `OData`-style parameters.
///
/// The body is a top-level `JSON` array, as documented for the list endpoints.
fn list_response<T: serde::Serialize>(query: &ListQuery, items: Vec<T>) -> axum::response::Response {
    let rows = query.apply(to_rows(items));
    (StatusCode::OK, Json(rows)).into_response()
}
