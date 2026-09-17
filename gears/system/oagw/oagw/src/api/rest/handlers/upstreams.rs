//! Upstream management endpoints.

use axum::{Extension, body::Bytes, extract::{Path, Query}};
use axum::http::Uri;
use axum::response::IntoResponse;

use crate::api::rest::dto::{ListEnvelope, UpstreamRequestDto, UpstreamResponseDto};
use crate::api::rest::error::ApiError;
use crate::api::rest::extractors::{ApiState, Caller, ListParams, ResourceId, Tenant};
use crate::api::rest::handlers::parse_body;
/// `GET /oagw/v1/upstreams`
pub async fn list(
    Extension(state): Extension<ApiState>,
    Tenant(tenant): Tenant,
    security: Caller,
    Query(params): Query<ListParams>,
) -> Result<axum::Json<ListEnvelope<UpstreamResponseDto>>, ApiError> {
    // The listing reports what the tenant inherits as well: an ancestor
    // definition carrying a shared field (`inherit` / `enforce`) is visible to
    // its descendants, a `private` one is not.
    let page = state
        .upstreams
        .list_visible(Some(&security.0), tenant, &params.into())
        .await
        .map_err(ApiError::from)?;
    let dtos: Vec<UpstreamResponseDto> = page
        .items
        .into_iter()
        .map(UpstreamResponseDto::from)
        .collect();
    Ok(axum::Json(ListEnvelope::from_vec(dtos, page.total)))
}

/// `POST /oagw/v1/upstreams`
pub async fn create(
    Extension(state): Extension<ApiState>,
    Tenant(tenant): Tenant,
    uri: Uri,
    body: Bytes,
) -> Result<axum::response::Response, ApiError> {
    let body: UpstreamRequestDto = parse_body(&body)?;
    let upstream = state
        .upstreams
        .create(tenant, body.into())
        .await
        .map_err(ApiError::from)?;
    let id = upstream.id.to_string();
    let dto = UpstreamResponseDto::from(upstream);
    Ok(toolkit::api::canonical_prelude::created_json(dto, &uri, &id).into_response())
}

/// `GET /oagw/v1/upstreams/{id}`
pub async fn get(
    Extension(state): Extension<ApiState>,
    Tenant(tenant): Tenant,
    Path(id): Path<ResourceId>,
) -> Result<axum::Json<UpstreamResponseDto>, ApiError> {
    let upstream = state
        .upstreams
        .get(tenant, id.0)
        .await
        .map_err(ApiError::from)?;
    Ok(axum::Json(UpstreamResponseDto::from(upstream)))
}

/// `PUT /oagw/v1/upstreams/{id}`
pub async fn update(
    Extension(state): Extension<ApiState>,
    Tenant(tenant): Tenant,
    Path(id): Path<ResourceId>,
    body: Bytes,
) -> Result<axum::Json<UpstreamResponseDto>, ApiError> {
    let body: UpstreamRequestDto = parse_body(&body)?;
    let upstream = state
        .upstreams
        .update(tenant, id.0, body.into())
        .await
        .map_err(ApiError::from)?;
    Ok(axum::Json(UpstreamResponseDto::from(upstream)))
}

/// `DELETE /oagw/v1/upstreams/{id}`
pub async fn delete(
    Extension(state): Extension<ApiState>,
    Tenant(tenant): Tenant,
    Path(id): Path<ResourceId>,
) -> Result<axum::http::StatusCode, ApiError> {
    state
        .upstreams
        .delete(tenant, id.0)
        .await
        .map_err(ApiError::from)?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}
