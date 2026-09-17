//! Handlers for `/oagw/v1/upstreams`.

use axum::Json;
use axum::extract::{Extension, Path};
use axum::http::Uri;
use axum::response::IntoResponse;
use toolkit::api::canonical_prelude::{created_json, no_content};

use super::SharedService;
use crate::api::rest::dto::{UpstreamListDto, UpstreamRequestDto, UpstreamResponseDto};
use crate::api::rest::error::ApiResult;
use crate::api::rest::extractors::{ListQueryParams, Tenant, require_path_id};
use crate::domain::dto::ListQuery;
use crate::domain::services::UpstreamDraft;

/// `POST /oagw/v1/upstreams` — create an upstream.
///
/// Returns `201` with the stored upstream and a `Location` header; `400` on a
/// validation failure and `409` when the alias is already used in the tenant.
pub async fn create_upstream(
    Extension(svc): Extension<SharedService>,
    Tenant(tenant): Tenant,
    uri: Uri,
    Json(request): Json<UpstreamRequestDto>,
) -> ApiResult<impl IntoResponse> {
    let draft = UpstreamDraft::try_from(&request)?;
    let upstream = svc.create_upstream(tenant, draft)?;
    Ok(created_json(
        UpstreamResponseDto::from(&upstream),
        &uri,
        &upstream.id.to_string(),
    ))
}

/// `GET /oagw/v1/upstreams` — list the tenant's upstreams.
pub async fn list_upstreams(
    Extension(svc): Extension<SharedService>,
    Tenant(tenant): Tenant,
    ListQueryParams(params): ListQueryParams,
) -> ApiResult<Json<UpstreamListDto>> {
    let query = list_query(&svc, &params)?;
    let page = svc.list_upstreams(tenant, &query)?;
    Ok(Json(UpstreamListDto {
        items: page.items.iter().map(UpstreamResponseDto::from).collect(),
        total: u64::try_from(page.total).unwrap_or(u64::MAX),
    }))
}

/// `GET /oagw/v1/upstreams/{id}` — fetch one upstream.
pub async fn get_upstream(
    Extension(svc): Extension<SharedService>,
    Tenant(tenant): Tenant,
    Path(id): Path<String>,
) -> ApiResult<Json<UpstreamResponseDto>> {
    let id = require_path_id(&id)?;
    let upstream = svc.get_upstream(tenant, id)?;
    Ok(Json(UpstreamResponseDto::from(&upstream)))
}

/// `PUT /oagw/v1/upstreams/{id}` — replace an upstream (full replacement).
pub async fn replace_upstream(
    Extension(svc): Extension<SharedService>,
    Tenant(tenant): Tenant,
    Path(id): Path<String>,
    Json(request): Json<UpstreamRequestDto>,
) -> ApiResult<Json<UpstreamResponseDto>> {
    let id = require_path_id(&id)?;
    let draft = UpstreamDraft::try_from(&request)?;
    let upstream = svc.replace_upstream(tenant, id, draft)?;
    Ok(Json(UpstreamResponseDto::from(&upstream)))
}

/// `DELETE /oagw/v1/upstreams/{id}` — delete an upstream (and its routes).
pub async fn delete_upstream(
    Extension(svc): Extension<SharedService>,
    Tenant(tenant): Tenant,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let id = require_path_id(&id)?;
    svc.delete_upstream(tenant, id)?;
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
