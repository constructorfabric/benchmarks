//! Handlers for `/oagw/v1/routes`.

use axum::Json;
use axum::extract::{Extension, Path};
use axum::http::Uri;
use axum::response::IntoResponse;
use toolkit::api::canonical_prelude::{created_json, no_content};

use super::SharedService;
use crate::api::rest::dto::{RouteListDto, RouteRequestDto, RouteResponseDto, RouteUpdateDto};
use crate::api::rest::error::ApiResult;
use crate::api::rest::extractors::{ListQueryParams, Tenant, require_path_id};
use crate::domain::dto::ListQuery;
use crate::domain::services::RouteDraft;

/// `POST /oagw/v1/routes` — create a route.
///
/// Returns `201` with the stored route; `400` when `upstream_id` does not
/// reference an upstream of the tenant or the match is invalid, and `409` when
/// the match duplicates an existing route of the same upstream.
pub async fn create_route(
    Extension(svc): Extension<SharedService>,
    Tenant(tenant): Tenant,
    uri: Uri,
    Json(request): Json<RouteRequestDto>,
) -> ApiResult<impl IntoResponse> {
    let draft = RouteDraft::try_from(&request)?;
    let route = svc.create_route(tenant, draft)?;
    Ok(created_json(
        RouteResponseDto::from(&route),
        &uri,
        &route.id.to_string(),
    ))
}

/// `GET /oagw/v1/routes` — list the tenant's routes.
pub async fn list_routes(
    Extension(svc): Extension<SharedService>,
    Tenant(tenant): Tenant,
    ListQueryParams(params): ListQueryParams,
) -> ApiResult<Json<RouteListDto>> {
    let query = list_query(&svc, &params)?;
    let page = svc.list_routes(tenant, &query)?;
    Ok(Json(RouteListDto {
        items: page.items.iter().map(RouteResponseDto::from).collect(),
        total: u64::try_from(page.total).unwrap_or(u64::MAX),
    }))
}

/// `GET /oagw/v1/routes/{id}` — fetch one route.
pub async fn get_route(
    Extension(svc): Extension<SharedService>,
    Tenant(tenant): Tenant,
    Path(id): Path<String>,
) -> ApiResult<Json<RouteResponseDto>> {
    let id = require_path_id(&id)?;
    let route = svc.get_route(tenant, id)?;
    Ok(Json(RouteResponseDto::from(&route)))
}

/// `PUT /oagw/v1/routes/{id}` — replace a route.
///
/// `upstream_id` is immutable: repeating the current value is accepted, a
/// different value is rejected with `400`.
pub async fn replace_route(
    Extension(svc): Extension<SharedService>,
    Tenant(tenant): Tenant,
    Path(id): Path<String>,
    Json(request): Json<RouteUpdateDto>,
) -> ApiResult<Json<RouteResponseDto>> {
    let id = require_path_id(&id)?;
    let draft = RouteDraft::try_from(&request)?;
    let route = svc.replace_route(tenant, id, draft)?;
    Ok(Json(RouteResponseDto::from(&route)))
}

/// `DELETE /oagw/v1/routes/{id}` — delete a route.
pub async fn delete_route(
    Extension(svc): Extension<SharedService>,
    Tenant(tenant): Tenant,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let id = require_path_id(&id)?;
    svc.delete_route(tenant, id)?;
    Ok(no_content())
}

/// Build the clamped list query from the raw parameters and the gear config.
#[allow(clippy::result_large_err)] // ApiResult's Err is the gear's problem document
fn list_query(
    svc: &SharedService,
    params: &crate::domain::dto::ListParams,
) -> ApiResult<ListQuery> {
    Ok(ListQuery::from_parts(
        params,
        svc.config().management.default_top,
        svc.config().management.max_top,
    )?)
}
