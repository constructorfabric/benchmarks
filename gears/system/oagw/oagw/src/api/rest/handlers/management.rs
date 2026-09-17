//! Management API handlers: upstreams, routes and plugins.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, Path};
use axum::http::{StatusCode, Uri, header};
use axum::response::IntoResponse;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::{DomainError, ErrorKind};
use crate::domain::services::management::NewPlugin;
use crate::domain::services::management::normalize_requested_alias;

use super::super::dto::{
    ListParams, PluginDto, PluginRequest, PluginSourceResponse, RouteDto, RouteRequest,
    UpstreamDto, UpstreamRequest, parse_resource_id,
};
use super::super::error::ApiResult;
use super::super::state::OagwState;

/// Fields a list endpoint may project or filter on.
const UPSTREAM_FIELDS: &[&str] = &[
    "id",
    "tenant_id",
    "alias",
    "alias_explicit",
    "enabled",
    "tags",
    "server",
    "protocol",
    "auth",
    "headers",
    "plugins",
    "rate_limit",
    "cors",
    "created_at",
    "updated_at",
];
const ROUTE_FIELDS: &[&str] = &[
    "id",
    "tenant_id",
    "upstream_id",
    "enabled",
    "tags",
    "match",
    "plugins",
    "rate_limit",
    "cors",
    "priority",
    "created_at",
    "updated_at",
];
const PLUGIN_FIELDS: &[&str] = &[
    "id",
    "tenant_id",
    "name",
    "type",
    "config_schema",
    "created_at",
    "updated_at",
];

/// Resolve a path parameter that may be a bare UUID or a GTS identifier.
fn resource_id(raw: &str) -> Result<Uuid, DomainError> {
    parse_resource_id(raw).ok_or_else(|| {
        DomainError::validation(format!(
            "`{raw}` is not a resource id: use a UUID or a `gts.cf.core.oagw.*.v1~<uuid>` id"
        ))
    })
}

/// `POST /oagw/v1/upstreams`
///
/// # Errors
///
/// Returns a problem document on validation failure or alias conflict.
pub async fn create_upstream(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(ctx): Extension<SecurityContext>,
    uri: Uri,
    Json(body): Json<UpstreamRequest>,
) -> ApiResult<impl IntoResponse> {
    let tenant = ctx.subject_tenant_id();
    let alias = normalize_requested_alias(body.alias.as_deref())?;
    let record = state
        .control_plane
        .create_upstream(tenant, body.spec, alias.as_deref())?;
    let location = created_location(uri.path(), crate::ids::UPSTREAM_TYPE, record.id);
    Ok((
        StatusCode::CREATED,
        [(header::LOCATION, location)],
        Json(UpstreamDto::from(record)),
    ))
}

/// `GET /oagw/v1/upstreams`
///
/// # Errors
///
/// Returns a problem document for invalid `OData` parameters.
pub async fn list_upstreams(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(ctx): Extension<SecurityContext>,
    ListParams(params): ListParams,
) -> ApiResult<Json<Vec<serde_json::Value>>> {
    let tenant = ctx.subject_tenant_id();
    let items: Vec<UpstreamDto> = state
        .control_plane
        .list_upstreams(tenant)?
        .into_iter()
        .filter(|u| super::super::dto::matches_filter(&u, &params.filter))
        .map(UpstreamDto::from)
        .collect();
    Ok(Json(params.apply(items, UPSTREAM_FIELDS)?))
}

/// `GET /oagw/v1/upstreams/{id}`
///
/// # Errors
///
/// Returns a problem document when the id is malformed or the resource is
/// missing.
pub async fn get_upstream(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(raw_id): Path<String>,
) -> ApiResult<Json<UpstreamDto>> {
    let tenant = ctx.subject_tenant_id();
    let id = resource_id(&raw_id)?;
    let record = state
        .control_plane
        .get_upstream(tenant, id)?
        .ok_or_else(missing)?;
    Ok(Json(UpstreamDto::from(record)))
}

/// `PUT /oagw/v1/upstreams/{id}`
///
/// # Errors
///
/// Returns a problem document on validation failure, alias drift or conflict.
pub async fn replace_upstream(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(raw_id): Path<String>,
    Json(body): Json<UpstreamRequest>,
) -> ApiResult<Json<UpstreamDto>> {
    let tenant = ctx.subject_tenant_id();
    let id = resource_id(&raw_id)?;
    let alias = normalize_requested_alias(body.alias.as_deref())?;
    let record = state
        .control_plane
        .replace_upstream(tenant, id, body.spec, alias.as_deref())?;
    Ok(Json(UpstreamDto::from(record)))
}

/// `DELETE /oagw/v1/upstreams/{id}`
///
/// # Errors
///
/// Returns a problem document when the id is malformed.
pub async fn delete_upstream(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(raw_id): Path<String>,
) -> ApiResult<axum::http::StatusCode> {
    let tenant = ctx.subject_tenant_id();
    let id = resource_id(&raw_id)?;
    state.control_plane.delete_upstream(tenant, id)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /oagw/v1/routes`
///
/// # Errors
///
/// Returns a problem document on validation failure, unknown upstream or
/// match-rule conflict.
pub async fn create_route(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(ctx): Extension<SecurityContext>,
    uri: Uri,
    Json(body): Json<RouteRequest>,
) -> ApiResult<impl IntoResponse> {
    let tenant = ctx.subject_tenant_id();
    let upstream_id = resource_id(&body.upstream_id)?;
    let record = state
        .control_plane
        .create_route(tenant, upstream_id, body.spec)?;
    let location = created_location(uri.path(), crate::ids::ROUTE_TYPE, record.id);
    Ok((
        StatusCode::CREATED,
        [(header::LOCATION, location)],
        Json(RouteDto::from(record)),
    ))
}

/// `GET /oagw/v1/routes`
///
/// # Errors
///
/// Returns a problem document for invalid `OData` parameters.
pub async fn list_routes(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(ctx): Extension<SecurityContext>,
    ListParams(params): ListParams,
) -> ApiResult<Json<Vec<serde_json::Value>>> {
    let tenant = ctx.subject_tenant_id();
    let upstream_filter = params
        .filter
        .iter()
        .find(|(field, _)| field == "upstream_id")
        .and_then(|(_, value)| parse_resource_id(value));
    let items: Vec<RouteDto> = state
        .control_plane
        .list_routes(tenant, upstream_filter)?
        .into_iter()
        .filter(|r| super::super::dto::matches_filter(&r, &params.filter))
        .map(RouteDto::from)
        .collect();
    Ok(Json(params.apply(items, ROUTE_FIELDS)?))
}

/// `GET /oagw/v1/routes/{id}`
///
/// # Errors
///
/// Returns a problem document when the id is malformed or the resource is
/// missing.
pub async fn get_route(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(raw_id): Path<String>,
) -> ApiResult<Json<RouteDto>> {
    let tenant = ctx.subject_tenant_id();
    let id = resource_id(&raw_id)?;
    let record = state
        .control_plane
        .get_route(tenant, id)?
        .ok_or_else(missing)?;
    Ok(Json(RouteDto::from(record)))
}

/// `PUT /oagw/v1/routes/{id}`
///
/// # Errors
///
/// Returns a problem document on validation failure or conflict.
pub async fn replace_route(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(raw_id): Path<String>,
    Json(body): Json<RouteRequest>,
) -> ApiResult<Json<RouteDto>> {
    let tenant = ctx.subject_tenant_id();
    let id = resource_id(&raw_id)?;
    let upstream_id = resource_id(&body.upstream_id)?;
    let record = state
        .control_plane
        .replace_route(tenant, id, upstream_id, body.spec)?;
    Ok(Json(RouteDto::from(record)))
}

/// `DELETE /oagw/v1/routes/{id}`
///
/// # Errors
///
/// Returns a problem document when the id is malformed.
pub async fn delete_route(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(raw_id): Path<String>,
) -> ApiResult<axum::http::StatusCode> {
    let tenant = ctx.subject_tenant_id();
    let id = resource_id(&raw_id)?;
    state.control_plane.delete_route(tenant, id)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /oagw/v1/plugins`
///
/// # Errors
///
/// Returns a problem document on validation failure.
pub async fn create_plugin(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(ctx): Extension<SecurityContext>,
    uri: Uri,
    Json(body): Json<PluginRequest>,
) -> ApiResult<impl IntoResponse> {
    let tenant = ctx.subject_tenant_id();
    let record = state.control_plane.create_plugin(
        tenant,
        NewPlugin {
            name: body.name,
            kind: body.kind,
            config_schema: body.config_schema,
            source_code: body.source_code,
        },
    )?;
    let location = created_location(uri.path(), record.kind.base_type(), record.id);
    Ok((
        StatusCode::CREATED,
        [(header::LOCATION, location)],
        Json(PluginDto::from(record)),
    ))
}

/// `GET /oagw/v1/plugins`
///
/// # Errors
///
/// Returns a problem document for invalid `OData` parameters.
pub async fn list_plugins(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(ctx): Extension<SecurityContext>,
    ListParams(params): ListParams,
) -> ApiResult<Json<Vec<serde_json::Value>>> {
    let tenant = ctx.subject_tenant_id();
    let items: Vec<PluginDto> = state
        .control_plane
        .list_plugins(tenant)?
        .into_iter()
        .filter(|p| super::super::dto::matches_filter(&p, &params.filter))
        .map(PluginDto::from)
        .collect();
    Ok(Json(params.apply(items, PLUGIN_FIELDS)?))
}

/// `GET /oagw/v1/plugins/{id}`
///
/// # Errors
///
/// Returns a problem document when the id is malformed or the resource is
/// missing.
pub async fn get_plugin(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(raw_id): Path<String>,
) -> ApiResult<Json<PluginDto>> {
    let tenant = ctx.subject_tenant_id();
    let id = resource_id(&raw_id)?;
    let record = state
        .control_plane
        .get_plugin(tenant, id)?
        .ok_or_else(missing)?;
    Ok(Json(PluginDto::from(record)))
}

/// `DELETE /oagw/v1/plugins/{id}`
///
/// # Errors
///
/// Returns `409` when the plugin is still bound.
pub async fn delete_plugin(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(raw_id): Path<String>,
) -> ApiResult<axum::http::StatusCode> {
    let tenant = ctx.subject_tenant_id();
    let id = resource_id(&raw_id)?;
    let existed = state.control_plane.delete_plugin(tenant, id)?;
    if !existed {
        return Err(missing().into());
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /oagw/v1/plugins/{id}/source`
///
/// # Errors
///
/// Returns a problem document when the plugin is missing.
pub async fn get_plugin_source(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(raw_id): Path<String>,
) -> ApiResult<Json<PluginSourceResponse>> {
    let tenant = ctx.subject_tenant_id();
    let id = resource_id(&raw_id)?;
    let record = state
        .control_plane
        .get_plugin(tenant, id)?
        .ok_or_else(missing)?;
    Ok(Json(PluginSourceResponse {
        id: super::super::dto::format_resource_id(record.kind.base_type(), record.id),
        source: record.source_code,
    }))
}

/// `Location` header for a 201 response: collection path plus the GTS id.
fn created_location(collection: &str, base: &str, id: Uuid) -> String {
    format!(
        "{}/{}",
        collection.trim_end_matches('/'),
        crate::ids::format_id(base, id)
    )
}

fn missing() -> DomainError {
    DomainError::new(ErrorKind::ResourceNotFound, "resource does not exist")
}

#[cfg(test)]
#[path = "management_tests.rs"]
mod management_tests;
