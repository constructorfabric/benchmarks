//! Upstream handlers of the OAGW management REST surface.

use axum::Extension;
use axum::extract::Path;
use axum::http::HeaderMap;
use axum::http::Uri;
use axum::response::IntoResponse;

use super::{Service, path_uuid};
use crate::api::rest::dto::{UpstreamDto, UpstreamRequest};
use crate::api::rest::extractors::{JsonBody, UpstreamList};
use crate::domain::error::{ApiResult, OagwError};
use crate::domain::model::parse_upstream_id;
use toolkit::api::canonical_prelude::{created_json, no_content, ok_json};
use toolkit_security::SecurityContext;

/// The request id of a mutation, from the `x-request-id` header.
fn request_id(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
}

/// Missing upstream problem document for `id`.
fn missing(id: uuid::Uuid) -> OagwError {
    OagwError::not_found(format!(
        "upstream gts.cf.core.oagw.upstream.v1~{id} does not exist"
    ))
    .with_upstream_id(id)
}

/// `POST /oagw/v1/upstreams` — creates an upstream for the calling tenant.
///
/// # Errors
///
/// Returns the problem document reported by
/// [`ControlPlaneService::create_upstream`](crate::domain::services::ControlPlaneService::create_upstream).
pub async fn create_upstream(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Service>,
    headers: HeaderMap,
    JsonBody(request): JsonBody<UpstreamRequest>,
) -> ApiResult<impl IntoResponse> {
    let input = request.as_input();
    let created = svc
        .create_upstream(&ctx, request_id(&headers), &input)
        .await?;
    Ok(created_json(
        UpstreamDto::from(&*created),
        &uri,
        &created.id.to_string(),
    ))
}

/// `GET /oagw/v1/upstreams` — lists the upstreams of the calling tenant.
///
/// # Errors
///
/// Returns the problem document reported by the OData query parser.
pub async fn list_upstreams(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Service>,
    UpstreamList(query): UpstreamList,
) -> ApiResult<impl IntoResponse> {
    let items = svc
        .list_upstreams(&ctx)
        .into_iter()
        .map(|upstream| UpstreamDto::from(&*upstream))
        .collect();
    let page = query.apply_values(items)?;
    Ok(ok_json(page))
}

/// `GET /oagw/v1/upstreams/{id}` — reads one upstream of the calling tenant.
///
/// # Errors
///
/// Returns [`OagwError::NotFound`] when the calling tenant does not own the
/// upstream.
pub async fn get_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Service>,
    Path(raw_id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let id = path_uuid(&raw_id, "upstream", parse_upstream_id)?;
    let upstream = svc.get_upstream(&ctx, id).ok_or_else(|| missing(id))?;
    Ok(ok_json(UpstreamDto::from(&*upstream)))
}

/// `PUT /oagw/v1/upstreams/{id}` — replaces an upstream of the calling tenant.
///
/// # Errors
///
/// Returns the problem document reported by
/// [`ControlPlaneService::replace_upstream`](crate::domain::services::ControlPlaneService::replace_upstream).
pub async fn replace_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Service>,
    Path(raw_id): Path<String>,
    headers: HeaderMap,
    JsonBody(request): JsonBody<UpstreamRequest>,
) -> ApiResult<impl IntoResponse> {
    let id = path_uuid(&raw_id, "upstream", parse_upstream_id)?;
    let input = request.as_input();
    let replaced = svc
        .replace_upstream(&ctx, request_id(&headers), id, &input)
        .await?;
    Ok(ok_json(UpstreamDto::from(&*replaced)))
}

/// `DELETE /oagw/v1/upstreams/{id}` — deletes an upstream and its routes.
///
/// # Errors
///
/// Returns [`OagwError::NotFound`] when the calling tenant does not own the
/// upstream.
pub async fn delete_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Service>,
    Path(raw_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<impl IntoResponse> {
    let id = path_uuid(&raw_id, "upstream", parse_upstream_id)?;
    svc.delete_upstream(&ctx, request_id(&headers), id)?;
    Ok(no_content())
}
