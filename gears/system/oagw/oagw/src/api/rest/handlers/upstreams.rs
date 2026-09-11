//! Upstream management handlers.
//!
//! `POST`/`PUT` bodies are [`UpstreamCreateRequest`]; every response carries the
//! full [`UpstreamDto`]. List operations narrow the visible set with the
//! platform's parsed `OData` query before it is returned.

use std::sync::Arc;

use axum::Extension;
use axum::Json;
use axum::extract::Path;
use axum::response::IntoResponse;
use serde_json::Value;
use toolkit::api::odata::OData;

use crate::api::rest::dto::{UpstreamCreateRequest, UpstreamDto, UpstreamListDto};
use crate::api::rest::error::ApiResult;
use crate::api::rest::extractors::{Action, AuthenticatedSubject, JsonBody, require_permission};
use crate::api::rest::records;
use crate::domain::services::control_plane::ControlPlaneService;
use crate::gts_helpers::UPSTREAM_GTS_ID;

/// `POST /oagw/v1/upstreams` — register an endpoint pool.
/// # Errors
///
/// - `Unauthorized` when the subject lacks the permission the call needs
/// - the domain error the operation itself fails with
pub async fn create_upstream(
    subject: AuthenticatedSubject,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    JsonBody(request): JsonBody<UpstreamCreateRequest>,
) -> ApiResult<impl IntoResponse> {
    require_permission(subject.context(), Action::Write, UPSTREAM_GTS_ID)?;

    let upstream = svc
        .create_upstream(subject.context(), request.into_spec())
        .await?;
    Ok((http::StatusCode::CREATED, Json(UpstreamDto::from(upstream))))
}

/// `GET /oagw/v1/upstreams` — list the upstreams visible to the caller.
/// # Errors
///
/// - `Unauthorized` when the subject lacks the permission the call needs
/// - the domain error the operation itself fails with
/// - `NotFound` when the identifier does not name an upstream of this tenant
pub async fn list_upstreams(
    subject: AuthenticatedSubject,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    OData(query): OData,
) -> ApiResult<Json<UpstreamListDto>> {
    require_permission(subject.context(), Action::Read, UPSTREAM_GTS_ID)?;

    let upstreams = svc.list_upstreams(subject.context()).await?;
    let records: Vec<Value> = upstreams
        .into_iter()
        .map(|upstream| as_value(UpstreamDto::from(upstream)))
        .collect();
    let (items, total_count) = records::page(records, &query);
    Ok(Json(UpstreamListDto { items, total_count }))
}

/// `GET /oagw/v1/upstreams/{id}` — read one upstream.
/// # Errors
///
/// - `Unauthorized` when the subject lacks the permission the call needs
/// - the domain error the operation itself fails with
/// - `NotFound` when the identifier does not name an upstream of this tenant
pub async fn get_upstream(
    subject: AuthenticatedSubject,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Path(id): Path<uuid::Uuid>,
) -> ApiResult<Json<UpstreamDto>> {
    require_permission(subject.context(), Action::Read, UPSTREAM_GTS_ID)?;

    let upstream = svc.get_upstream(subject.context(), id).await?;
    Ok(Json(UpstreamDto::from(upstream)))
}

/// `PUT /oagw/v1/upstreams/{id}` — replace an upstream; the alias is immutable.
/// # Errors
///
/// - `Unauthorized` when the subject lacks the permission the call needs
/// - the domain error the operation itself fails with
/// - `NotFound` when the identifier does not name an upstream of this tenant
pub async fn replace_upstream(
    subject: AuthenticatedSubject,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Path(id): Path<uuid::Uuid>,
    JsonBody(request): JsonBody<UpstreamCreateRequest>,
) -> ApiResult<Json<UpstreamDto>> {
    require_permission(subject.context(), Action::Write, UPSTREAM_GTS_ID)?;

    let upstream = svc
        .replace_upstream(subject.context(), id, request.into_spec())
        .await?;
    Ok(Json(UpstreamDto::from(upstream)))
}

/// `DELETE /oagw/v1/upstreams/{id}` — remove an upstream no route references.
/// # Errors
///
/// - `Unauthorized` when the subject lacks the permission the call needs
/// - the domain error the operation itself fails with
/// - `NotFound` when the identifier does not name an upstream of this tenant
pub async fn delete_upstream(
    subject: AuthenticatedSubject,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Path(id): Path<uuid::Uuid>,
) -> ApiResult<impl IntoResponse> {
    require_permission(subject.context(), Action::Delete, UPSTREAM_GTS_ID)?;

    svc.delete_upstream(subject.context(), id).await?;
    Ok(http::StatusCode::NO_CONTENT)
}

/// Serialize a DTO, falling back to `null` rather than failing the list.
///
/// A `Vec<UpstreamDto>` carries only strings, numbers, booleans, arrays and
/// objects, so the fall-back is unreachable in practice; the handler still
/// cannot `unwrap` in it.
fn as_value<T: serde::Serialize>(value: T) -> Value {
    serde_json::to_value(&value).unwrap_or(Value::Null)
}
