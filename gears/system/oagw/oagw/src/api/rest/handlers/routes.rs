//! Management handlers for `/oagw/v1/routes`.

use axum::Extension;
use axum::Json;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use toolkit_security::SecurityContext;

use crate::api::rest::dto::{RouteDto, UpsertRouteDto, route_from_dto};
use crate::api::rest::handlers::upstreams::ListQuery;
use crate::domain::service::ControlPlaneService;

/// `POST /oagw/v1/routes`
///
/// # Errors
///
/// Returns 400 on validation failure, 404 when the upstream is not addressable
/// and 409 on a duplicate match rule.
pub async fn create_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<ControlPlaneService>,
    Json(body): Json<UpsertRouteDto>,
) -> Result<(StatusCode, Json<RouteDto>), crate::domain::error::DomainError> {
    let created = svc
        .create_route(ctx.subject_tenant_id(), route_from_dto(&body))
        .await?;
    Ok((StatusCode::CREATED, Json(RouteDto::from(created))))
}

/// `GET /oagw/v1/routes`
///
/// # Errors
///
/// Propagates store failures.
pub async fn list_routes(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<ControlPlaneService>,
    Query(query): Query<ListQuery>,
) -> Result<Json<Vec<RouteDto>>, crate::domain::error::DomainError> {
    let rows = svc
        .list_routes(ctx.subject_tenant_id(), &query.into_filter())
        .await?;
    Ok(Json(rows.into_iter().map(RouteDto::from).collect()))
}

/// `GET /oagw/v1/routes/{id}`
///
/// # Errors
///
/// Returns 404 when absent or owned by another tenant.
pub async fn get_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<ControlPlaneService>,
    Path(id): Path<uuid::Uuid>,
) -> Result<Json<RouteDto>, crate::domain::error::DomainError> {
    Ok(Json(RouteDto::from(
        svc.get_route(ctx.subject_tenant_id(), id).await?,
    )))
}

/// `PUT /oagw/v1/routes/{id}`
///
/// # Errors
///
/// Returns 404 when absent, 400 on a malformed match block and 409 on a
/// duplicate match rule. `upstream_id` is ignored on replace.
pub async fn replace_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<ControlPlaneService>,
    Path(id): Path<uuid::Uuid>,
    Json(body): Json<UpsertRouteDto>,
) -> Result<Json<RouteDto>, crate::domain::error::DomainError> {
    let updated = svc
        .replace_route(ctx.subject_tenant_id(), id, route_from_dto(&body))
        .await?;
    Ok(Json(RouteDto::from(updated)))
}

/// `DELETE /oagw/v1/routes/{id}`
///
/// # Errors
///
/// Returns 404 when absent or owned by another tenant.
pub async fn delete_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<ControlPlaneService>,
    Path(id): Path<uuid::Uuid>,
) -> Result<impl IntoResponse, crate::domain::error::DomainError> {
    svc.delete_route(ctx.subject_tenant_id(), id).await?;
    Ok(StatusCode::NO_CONTENT)
}
