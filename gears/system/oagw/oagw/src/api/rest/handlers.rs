//! Control Plane REST handlers (upstream / route / plugin CRUD).
//!
//! Every handler is tenant-scoped through the [`SecurityContext`] injected by
//! the platform authentication middleware; failures are mapped to RFC 9457
//! problem documents by [`crate::api::rest::error::OagwProblem`]. Bodies,
//! query strings and path parameters are extracted through the converting
//! extractors ([`ValidJson`], [`ValidQuery`], [`ValidPath`]) so that a
//! malformed request is reported as a canonical problem document instead of
//! axum's built-in `text/plain` rejection.

use std::sync::Arc;

use axum::Json;
use axum::extract::Extension;
use axum::http::StatusCode;
use toolkit_security::SecurityContext;

use crate::domain::error::DomainError;
use crate::domain::gts::plugin_instance_id;
use crate::domain::model::{Plugin, PluginType, Route, Upstream};
use crate::domain::services::management::{ControlPlaneService, ListQuery, PluginSource};
use crate::infra::controlplane::ControlPlaneServiceImpl;

use super::dto::{
    ListParamsDto, PluginCreateDto, PluginListParams, PluginSourceDto, RouteCreateDto,
    RouteUpdateDto, UpstreamBodyDto,
};
use super::error::{OagwProblem, RequestPath, ValidJson, ValidPath, ValidQuery};

/// The shared Control Plane handle injected into the router.
pub type ControlPlane = Arc<ControlPlaneServiceImpl>;

/// Result type of every management handler.
pub type Outcome<T> = Result<(StatusCode, Json<T>), OagwProblem>;

/// Maps a domain error onto a problem document pointing at `path`.
fn failed(path: &str) -> impl Fn(DomainError) -> OagwProblem + '_ {
    move |error| OagwProblem::at(path, error)
}

/// Applies `$select` to a collection.
///
/// [`crate::domain::services::management::ListQuery`] carries the requested
/// field names but returns whole resources, so the transport layer owns the
/// projection. With no `$select` (the default) the items are serialized whole,
/// which is the only case existing clients rely on.
fn selected<T: serde::Serialize>(
    items: Vec<T>,
    select: Option<&str>,
) -> Outcome<Vec<serde_json::Value>> {
    let fields = select.map(|expression| {
        expression
            .split(',')
            .map(str::trim)
            .filter(|field| !field.is_empty())
            .map(ToOwned::to_owned)
            .collect::<Vec<String>>()
    });
    let projected = items
        .iter()
        .map(|item| toolkit::api::select::apply_select(item, fields.as_deref()))
        .collect();
    Ok((StatusCode::OK, Json(projected)))
}

/// `POST /upstreams`.
///
/// # Errors
///
/// Validation, alias derivation and uniqueness failures.
pub async fn create_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<ControlPlane>,
    RequestPath(path): RequestPath,
    ValidJson(body): ValidJson<UpstreamBodyDto>,
) -> Outcome<Upstream> {
    let created = svc
        .create_upstream(&ctx, body.into_upstream())
        .await
        .map_err(failed(&path))?;
    Ok((StatusCode::CREATED, Json(created)))
}

/// `GET /upstreams`.
///
/// # Errors
///
/// Malformed filter expressions.
pub async fn list_upstreams(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<ControlPlane>,
    RequestPath(path): RequestPath,
    ValidQuery(params): ValidQuery<ListParamsDto>,
) -> Outcome<Vec<serde_json::Value>> {
    let query: ListQuery = params.to_list_query();
    let items = svc
        .list_upstreams(&ctx, &query)
        .await
        .map_err(failed(&path))?;
    selected(items, params.select.as_deref())
}

/// `GET /upstreams/{id}`.
///
/// # Errors
///
/// 404 when the upstream is not owned by the caller.
pub async fn get_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<ControlPlane>,
    RequestPath(path): RequestPath,
    ValidPath(id): ValidPath<String>,
) -> Outcome<Upstream> {
    let found = svc.get_upstream(&ctx, &id).await.map_err(failed(&path))?;
    Ok((StatusCode::OK, Json(found)))
}

/// `PUT /upstreams/{id}`.
///
/// # Errors
///
/// Validation and immutability failures.
pub async fn replace_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<ControlPlane>,
    RequestPath(path): RequestPath,
    ValidPath(id): ValidPath<String>,
    ValidJson(body): ValidJson<UpstreamBodyDto>,
) -> Outcome<Upstream> {
    let replaced = svc
        .replace_upstream(&ctx, &id, body.into_upstream())
        .await
        .map_err(failed(&path))?;
    Ok((StatusCode::OK, Json(replaced)))
}

/// `DELETE /upstreams/{id}`.
///
/// # Errors
///
/// 404 when the upstream is not owned by the caller.
pub async fn delete_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<ControlPlane>,
    RequestPath(path): RequestPath,
    ValidPath(id): ValidPath<String>,
) -> Result<StatusCode, OagwProblem> {
    svc.delete_upstream(&ctx, &id)
        .await
        .map_err(failed(&path))?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /routes`.
///
/// # Errors
///
/// Validation, upstream ownership and match-uniqueness failures.
pub async fn create_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<ControlPlane>,
    RequestPath(path): RequestPath,
    ValidJson(body): ValidJson<RouteCreateDto>,
) -> Outcome<Route> {
    let created = svc
        .create_route(&ctx, body.into_route())
        .await
        .map_err(failed(&path))?;
    Ok((StatusCode::CREATED, Json(created)))
}

/// `GET /routes`.
///
/// # Errors
///
/// Malformed filter expressions.
pub async fn list_routes(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<ControlPlane>,
    RequestPath(path): RequestPath,
    ValidQuery(params): ValidQuery<ListParamsDto>,
) -> Outcome<Vec<serde_json::Value>> {
    let query: ListQuery = params.to_list_query();
    let items = svc.list_routes(&ctx, &query).await.map_err(failed(&path))?;
    selected(items, params.select.as_deref())
}

/// `GET /routes/{id}`.
///
/// # Errors
///
/// 404 when the route is not owned by the caller.
pub async fn get_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<ControlPlane>,
    RequestPath(path): RequestPath,
    ValidPath(id): ValidPath<String>,
) -> Outcome<Route> {
    let found = svc.get_route(&ctx, &id).await.map_err(failed(&path))?;
    Ok((StatusCode::OK, Json(found)))
}

/// `PUT /routes/{id}`.
///
/// # Errors
///
/// Validation and match-uniqueness failures.
pub async fn replace_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<ControlPlane>,
    RequestPath(path): RequestPath,
    ValidPath(id): ValidPath<String>,
    ValidJson(body): ValidJson<RouteUpdateDto>,
) -> Outcome<Route> {
    let replaced = svc
        .replace_route(&ctx, &id, body.into_route())
        .await
        .map_err(failed(&path))?;
    Ok((StatusCode::OK, Json(replaced)))
}

/// `DELETE /routes/{id}`.
///
/// # Errors
///
/// 404 when the route is not owned by the caller.
pub async fn delete_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<ControlPlane>,
    RequestPath(path): RequestPath,
    ValidPath(id): ValidPath<String>,
) -> Result<StatusCode, OagwProblem> {
    svc.delete_route(&ctx, &id).await.map_err(failed(&path))?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /plugins`.
///
/// # Errors
///
/// Validation and name-uniqueness failures.
pub async fn create_plugin(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<ControlPlane>,
    RequestPath(path): RequestPath,
    ValidJson(body): ValidJson<PluginCreateDto>,
) -> Outcome<Plugin> {
    let created = svc
        .create_plugin(&ctx, body.into_plugin())
        .await
        .map_err(failed(&path))?;
    Ok((StatusCode::CREATED, Json(created)))
}

/// `GET /plugins`.
///
/// # Errors
///
/// Malformed filter expressions.
pub async fn list_plugins(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<ControlPlane>,
    RequestPath(path): RequestPath,
    ValidQuery(params): ValidQuery<PluginListParams>,
) -> Outcome<Vec<serde_json::Value>> {
    // The OData parameters reach the service unchanged; the `type` /
    // `plugin_type` shorthand is applied in addition to the forwarded filter.
    let query: ListQuery = params.to_list_query();
    let plugin_type = params.plugin_type.map(PluginType::from);
    let items = svc
        .list_plugins(&ctx, &query, plugin_type)
        .await
        .map_err(failed(&path))?;
    selected(items, params.select.as_deref())
}

/// `GET /plugins/{id}`.
///
/// # Errors
///
/// 404 when the plugin is not owned by the caller.
pub async fn get_plugin(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<ControlPlane>,
    RequestPath(path): RequestPath,
    ValidPath(id): ValidPath<String>,
) -> Outcome<Plugin> {
    let found = svc.get_plugin(&ctx, &id).await.map_err(failed(&path))?;
    Ok((StatusCode::OK, Json(found)))
}

/// `DELETE /plugins/{id}`.
///
/// # Errors
///
/// 409 [`crate::domain::error::DomainError::PluginInUse`] with the
/// referencing resources.
pub async fn delete_plugin(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<ControlPlane>,
    RequestPath(path): RequestPath,
    ValidPath(id): ValidPath<String>,
) -> Result<StatusCode, OagwProblem> {
    svc.delete_plugin(&ctx, &id).await.map_err(failed(&path))?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /plugins/{id}/source`.
///
/// # Errors
///
/// 404 when the plugin is not owned by the caller.
pub async fn get_plugin_source(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<ControlPlane>,
    RequestPath(path): RequestPath,
    ValidPath(id): ValidPath<String>,
) -> Outcome<PluginSourceDto> {
    let source: PluginSource = svc
        .get_plugin_source(&ctx, &id)
        .await
        .map_err(failed(&path))?;
    Ok((
        StatusCode::OK,
        Json(PluginSourceDto {
            plugin_id: plugin_instance_id(source.plugin_type, parse_plugin_id(&source.plugin_id)),
            plugin_type: source.plugin_type.into(),
            source_code: source.source_code,
        }),
    ))
}

/// Parses a plugin identifier, accepting both the bare UUID tail and a full
/// GTS identifier.
fn parse_plugin_id(plugin_id: &str) -> uuid::Uuid {
    crate::domain::gts::uuid_from_instance_id(plugin_id).unwrap_or_default()
}
