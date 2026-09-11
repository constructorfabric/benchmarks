//! Route management handlers.

use std::sync::Arc;

use axum::Extension;
use axum::Json;
use axum::extract::Path;
use axum::response::IntoResponse;
use serde_json::Value;
use toolkit::api::odata::OData;

use crate::api::rest::dto::{RouteCreateRequest, RouteDto, RouteListDto};
use crate::api::rest::error::ApiResult;
use crate::api::rest::extractors::{Action, AuthenticatedSubject, JsonBody, require_permission};
use crate::api::rest::records;
use crate::domain::services::control_plane::ControlPlaneService;
use crate::gts_helpers::ROUTE_GTS_ID;

/// `POST /oagw/v1/routes` — bind a path to an upstream.
/// # Errors
///
/// - `Unauthorized` when the subject lacks the permission the call needs
/// - the domain error the operation itself fails with
pub async fn create_route(
    subject: AuthenticatedSubject,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    JsonBody(request): JsonBody<RouteCreateRequest>,
) -> ApiResult<impl IntoResponse> {
    require_permission(subject.context(), Action::Write, ROUTE_GTS_ID)?;

    let route = svc
        .create_route(subject.context(), request.into_spec())
        .await?;
    Ok((http::StatusCode::CREATED, Json(RouteDto::from(route))))
}

/// `GET /oagw/v1/routes` — list the caller's routes.
/// # Errors
///
/// - `Unauthorized` when the subject lacks the permission the call needs
/// - the domain error the operation itself fails with
/// - `NotFound` when the identifier does not name a route of this tenant
pub async fn list_routes(
    subject: AuthenticatedSubject,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    OData(query): OData,
) -> ApiResult<Json<RouteListDto>> {
    require_permission(subject.context(), Action::Read, ROUTE_GTS_ID)?;

    let routes = svc.list_routes(subject.context()).await?;
    let records: Vec<Value> = routes
        .into_iter()
        .map(|route| as_value(RouteDto::from(route)))
        .collect();
    let (items, total_count) = records::page(records, &query);
    Ok(Json(RouteListDto { items, total_count }))
}

/// `GET /oagw/v1/routes/{id}` — read one route.
/// # Errors
///
/// - `Unauthorized` when the subject lacks the permission the call needs
/// - the domain error the operation itself fails with
/// - `NotFound` when the identifier does not name a route of this tenant
pub async fn get_route(
    subject: AuthenticatedSubject,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Path(id): Path<uuid::Uuid>,
) -> ApiResult<Json<RouteDto>> {
    require_permission(subject.context(), Action::Read, ROUTE_GTS_ID)?;

    let route = svc.get_route(subject.context(), id).await?;
    Ok(Json(RouteDto::from(route)))
}

/// `PUT /oagw/v1/routes/{id}` — replace a route.
/// # Errors
///
/// - `Unauthorized` when the subject lacks the permission the call needs
/// - the domain error the operation itself fails with
/// - `NotFound` when the identifier does not name a route of this tenant
pub async fn replace_route(
    subject: AuthenticatedSubject,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Path(id): Path<uuid::Uuid>,
    JsonBody(request): JsonBody<RouteCreateRequest>,
) -> ApiResult<Json<RouteDto>> {
    require_permission(subject.context(), Action::Write, ROUTE_GTS_ID)?;

    let route = svc
        .replace_route(subject.context(), id, request.into_spec())
        .await?;
    Ok(Json(RouteDto::from(route)))
}

/// `DELETE /oagw/v1/routes/{id}` — remove a route and its bindings.
/// # Errors
///
/// - `Unauthorized` when the subject lacks the permission the call needs
/// - the domain error the operation itself fails with
/// - `NotFound` when the identifier does not name a route of this tenant
pub async fn delete_route(
    subject: AuthenticatedSubject,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Path(id): Path<uuid::Uuid>,
) -> ApiResult<impl IntoResponse> {
    require_permission(subject.context(), Action::Delete, ROUTE_GTS_ID)?;

    svc.delete_route(subject.context(), id).await?;
    Ok(http::StatusCode::NO_CONTENT)
}

/// Serialize a DTO, falling back to `null` rather than failing the list.
fn as_value<T: serde::Serialize>(value: T) -> Value {
    serde_json::to_value(&value).unwrap_or(Value::Null)
}
