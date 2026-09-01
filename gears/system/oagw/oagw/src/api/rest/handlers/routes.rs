//! Route handlers of the OAGW management REST surface.

use axum::Extension;
use axum::extract::Path;
use axum::http::HeaderMap;
use axum::http::Uri;
use axum::response::IntoResponse;

use super::{Service, path_uuid};
use crate::api::rest::dto::{CreateRouteRequest, ReplaceRouteRequest, RouteDto};
use crate::api::rest::extractors::{JsonBody, RouteList};
use crate::domain::error::{ApiResult, OagwError};
use crate::domain::model::parse_route_id;
use toolkit::api::canonical_prelude::{created_json, no_content, ok_json};
use toolkit_security::SecurityContext;

/// The request id of a mutation, from the `x-request-id` header.
fn request_id(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
}

/// Missing route problem document for `id`.
fn missing_route(id: uuid::Uuid) -> OagwError {
    OagwError::not_found(format!(
        "route gts.cf.core.oagw.route.v1~{id} does not exist"
    ))
}

/// `POST /oagw/v1/routes` — creates a route under an owned upstream.
///
/// # Errors
///
/// Returns the problem document reported by
/// [`ControlPlaneService::create_route`](crate::domain::services::ControlPlaneService::create_route).
pub async fn create_route(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Service>,
    headers: HeaderMap,
    JsonBody(request): JsonBody<CreateRouteRequest>,
) -> ApiResult<impl IntoResponse> {
    let upstream_id = request.upstream_id;
    let input = request.as_input();
    let created = svc
        .create_route(&ctx, request_id(&headers), upstream_id, &input)
        .await?;
    Ok(created_json(
        RouteDto::from(&*created),
        &uri,
        &created.id.to_string(),
    ))
}

/// `GET /oagw/v1/routes` — lists the routes of the calling tenant.
///
/// # Errors
///
/// Returns the problem document reported by the OData query parser.
pub async fn list_routes(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Service>,
    RouteList(query): RouteList,
) -> ApiResult<impl IntoResponse> {
    let items = svc
        .list_routes(&ctx)
        .into_iter()
        .map(|route| RouteDto::from(&*route))
        .collect();
    let page = query.apply_values(items)?;
    Ok(ok_json(page))
}

/// `GET /oagw/v1/routes/{id}` — reads one route of the calling tenant.
///
/// # Errors
///
/// Returns [`OagwError::NotFound`] when the calling tenant does not own the
/// route.
pub async fn get_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Service>,
    Path(raw_id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let id = path_uuid(&raw_id, "route", parse_route_id)?;
    let route = svc.get_route(&ctx, id).ok_or_else(|| missing_route(id))?;
    Ok(ok_json(RouteDto::from(&*route)))
}

/// `PUT /oagw/v1/routes/{id}` — replaces a route; `upstream_id` is immutable.
///
/// # Errors
///
/// Returns the problem document reported by
/// [`ControlPlaneService::replace_route`](crate::domain::services::ControlPlaneService::replace_route).
pub async fn replace_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Service>,
    Path(raw_id): Path<String>,
    headers: HeaderMap,
    JsonBody(request): JsonBody<ReplaceRouteRequest>,
) -> ApiResult<impl IntoResponse> {
    let id = path_uuid(&raw_id, "route", parse_route_id)?;
    let input = request.as_input();
    let replaced = svc
        .replace_route(&ctx, request_id(&headers), id, &input)
        .await?;
    Ok(ok_json(RouteDto::from(&*replaced)))
}

/// `DELETE /oagw/v1/routes/{id}` — deletes a route of the calling tenant.
///
/// # Errors
///
/// Returns [`OagwError::NotFound`] when the calling tenant does not own the
/// route.
pub async fn delete_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Service>,
    Path(raw_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<impl IntoResponse> {
    let id = path_uuid(&raw_id, "route", parse_route_id)?;
    svc.delete_route(&ctx, request_id(&headers), id)?;
    Ok(no_content())
}
