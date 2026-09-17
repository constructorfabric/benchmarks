//! Route management endpoints.

use axum::{Extension, body::Bytes, extract::{Path, Query}};
use axum::http::Uri;
use axum::response::IntoResponse;

use crate::api::rest::dto::{ListEnvelope, RouteRequestDto, RouteResponseDto};
use crate::api::rest::error::ApiError;
use crate::api::rest::extractors::{ApiState, ListParams, ResourceId, Tenant};
use crate::api::rest::handlers::parse_body;

/// `GET /oagw/v1/routes`
pub async fn list(
    Extension(state): Extension<ApiState>,
    Tenant(tenant): Tenant,
    Query(params): Query<ListParams>,
) -> Result<axum::Json<ListEnvelope<RouteResponseDto>>, ApiError> {
    let page = state
        .routes
        .list(tenant, &params.into())
        .await
        .map_err(ApiError::from)?;
    let dtos: Vec<RouteResponseDto> = page.items.into_iter().map(RouteResponseDto::from).collect();
    Ok(axum::Json(ListEnvelope::from_vec(dtos, page.total)))
}

/// `GET /oagw/v1/upstreams/{id}/routes`
pub async fn list_for_upstream(
    Extension(state): Extension<ApiState>,
    Tenant(tenant): Tenant,
    Path(id): Path<ResourceId>,
) -> Result<axum::Json<ListEnvelope<RouteResponseDto>>, ApiError> {
    // 404 when the upstream itself is unknown in this tenant.
    let _ = state.upstreams.get(tenant, id.0).await.map_err(ApiError::from)?;
    let routes = state
        .routes
        .list_by_upstream(tenant, id.0)
        .await
        .map_err(ApiError::from)?;
    let total = routes.len();
    let dtos: Vec<RouteResponseDto> = routes.into_iter().map(RouteResponseDto::from).collect();
    Ok(axum::Json(ListEnvelope::from_vec(dtos, total)))
}

/// `POST /oagw/v1/routes`
pub async fn create(
    Extension(state): Extension<ApiState>,
    Tenant(tenant): Tenant,
    uri: Uri,
    body: Bytes,
) -> Result<axum::response::Response, ApiError> {
    let body: RouteRequestDto = parse_body(&body)?;
    let route = state
        .routes
        .create(tenant, body.into())
        .await
        .map_err(ApiError::from)?;
    let id = route.id.to_string();
    let dto = RouteResponseDto::from(route);
    Ok(toolkit::api::canonical_prelude::created_json(dto, &uri, &id).into_response())
}

/// `GET /oagw/v1/routes/{id}`
pub async fn get(
    Extension(state): Extension<ApiState>,
    Tenant(tenant): Tenant,
    Path(id): Path<ResourceId>,
) -> Result<axum::Json<RouteResponseDto>, ApiError> {
    let route = state.routes.get(tenant, id.0).await.map_err(ApiError::from)?;
    Ok(axum::Json(RouteResponseDto::from(route)))
}

/// `PUT /oagw/v1/routes/{id}`
pub async fn update(
    Extension(state): Extension<ApiState>,
    Tenant(tenant): Tenant,
    Path(id): Path<ResourceId>,
    body: Bytes,
) -> Result<axum::Json<RouteResponseDto>, ApiError> {
    let body: RouteRequestDto = parse_body(&body)?;
    let route = state
        .routes
        .update(tenant, id.0, body.into())
        .await
        .map_err(ApiError::from)?;
    Ok(axum::Json(RouteResponseDto::from(route)))
}

/// `DELETE /oagw/v1/routes/{id}`
pub async fn delete(
    Extension(state): Extension<ApiState>,
    Tenant(tenant): Tenant,
    Path(id): Path<ResourceId>,
) -> Result<axum::http::StatusCode, ApiError> {
    state.routes.delete(tenant, id.0).await.map_err(ApiError::from)?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

