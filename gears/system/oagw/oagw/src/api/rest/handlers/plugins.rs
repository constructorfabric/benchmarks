//! Handlers for `/oagw/v1/plugins`.

use axum::Json;
use axum::extract::{Extension, Path};
use axum::http::Uri;
use axum::response::IntoResponse;
use toolkit::api::canonical_prelude::{created_json, no_content};

use super::SharedService;
use crate::api::rest::dto::{
    PluginListDto, PluginRequestDto, PluginResponseDto, PluginSourceResponseDto,
};
use crate::api::rest::error::ApiResult;
use crate::api::rest::extractors::{ListQueryParams, Tenant, require_path_id};
use crate::domain::dto::ListQuery;
use crate::domain::services::PluginDraft;

/// `POST /oagw/v1/plugins` — create a custom (UUID-backed) plugin.
///
/// Plugins are immutable after creation, so there is no `PUT`.
pub async fn create_plugin(
    Extension(svc): Extension<SharedService>,
    Tenant(tenant): Tenant,
    uri: Uri,
    Json(request): Json<PluginRequestDto>,
) -> ApiResult<impl IntoResponse> {
    let draft = PluginDraft::from(&request);
    let plugin = svc.create_plugin(tenant, draft)?;
    Ok(created_json(
        PluginResponseDto::from(&plugin),
        &uri,
        &plugin.id.to_string(),
    ))
}

/// `GET /oagw/v1/plugins` — list the tenant's plugins.
pub async fn list_plugins(
    Extension(svc): Extension<SharedService>,
    Tenant(tenant): Tenant,
    ListQueryParams(params): ListQueryParams,
) -> ApiResult<Json<PluginListDto>> {
    let query = ListQuery::from_parts(
        &params,
        svc.config().management.default_top,
        svc.config().management.max_top,
    )?;
    let page = svc.list_plugins(tenant, &query)?;
    Ok(Json(PluginListDto {
        items: page.items.iter().map(PluginResponseDto::from).collect(),
        total: u64::try_from(page.total).unwrap_or(u64::MAX),
    }))
}

/// `GET /oagw/v1/plugins/{id}` — fetch one plugin.
pub async fn get_plugin(
    Extension(svc): Extension<SharedService>,
    Tenant(tenant): Tenant,
    Path(id): Path<String>,
) -> ApiResult<Json<PluginResponseDto>> {
    let id = require_path_id(&id)?;
    let plugin = svc.get_plugin(tenant, id)?;
    Ok(Json(PluginResponseDto::from(&plugin)))
}

/// `GET /oagw/v1/plugins/{id}/source` — the plugin's declared source.
pub async fn get_plugin_source(
    Extension(svc): Extension<SharedService>,
    Tenant(tenant): Tenant,
    Path(id): Path<String>,
) -> ApiResult<Json<PluginSourceResponseDto>> {
    let id = require_path_id(&id)?;
    let source = svc.get_plugin_source(tenant, id)?;
    let plugin = svc.get_plugin(tenant, id)?;
    let dto = PluginSourceResponseDto::from(&plugin);
    Ok(Json(PluginSourceResponseDto {
        gts_id: dto.gts_id,
        kind: source.kind.into(),
        source_code: source.source_code,
        language: source.language,
        location: source.location,
    }))
}

/// `DELETE /oagw/v1/plugins/{id}` — delete a plugin.
///
/// Returns `409` with a problem document naming the still-referencing
/// upstreams and routes while the plugin is bound, `204` once deleted.
pub async fn delete_plugin(
    Extension(svc): Extension<SharedService>,
    Tenant(tenant): Tenant,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let id = require_path_id(&id)?;
    svc.delete_plugin(tenant, id)?;
    Ok(no_content())
}
