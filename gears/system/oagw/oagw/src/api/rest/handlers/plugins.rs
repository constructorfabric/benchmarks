//! Tenant-defined plugin management and catalog endpoints.

use axum::{Extension, body::Bytes, extract::{Path, Query}};
use axum::http::Uri;
use axum::response::IntoResponse;

use crate::api::rest::dto::{ListEnvelope, PluginCatalog, PluginRequestDto, PluginResponseDto};
use crate::api::rest::error::ApiError;
use crate::domain::error::DomainError;
use crate::api::rest::extractors::{ApiState, ListParams, ResourceId, Tenant};
use crate::api::rest::handlers::parse_body;

/// `GET /oagw/v1/plugins`
pub async fn list(
    Extension(state): Extension<ApiState>,
    Tenant(tenant): Tenant,
    Query(params): Query<ListParams>,
) -> Result<axum::Json<ListEnvelope<PluginResponseDto>>, ApiError> {
    let page = state
        .plugins
        .list(tenant, &params.into())
        .await
        .map_err(ApiError::from)?;
    let dtos: Vec<PluginResponseDto> = page.items.into_iter().map(PluginResponseDto::from).collect();
    Ok(axum::Json(ListEnvelope::from_vec(dtos, page.total)))
}

/// `GET /oagw/v1/plugins/catalog`
///
/// Every documented plugin id is listed, built-in *and* catalog-only. The
/// catalog-only ids are repeated under `reserved` so a client can tell them
/// apart: binding one to a resource fails with a validation error.
pub async fn catalog(Extension(state): Extension<ApiState>) -> axum::Json<PluginCatalog> {
    let registry = &state.plugin_registry;
    // Built-in (resolvable) ids first, then the catalog-only ones, so a client
    // reading `*_names` sees the registry order documented in DESIGN.
    let mut auth = registry.auth_ids();
    let mut guard = registry.guard_ids();
    let mut transform = registry.transform_ids();

    let auth_names = names_of(&auth);
    let guard_names = names_of(&guard);
    let transform_names = names_of(&transform);

    let reserved_auth = reserved_ids(&crate::domain::dto::CATALOG_ONLY_AUTH_PLUGINS, &auth_names)
        .into_iter()
        .map(|name| crate::domain::gts_helpers::auth_plugin(&name))
        .collect::<Vec<_>>();
    let reserved_guard = reserved_ids(&crate::domain::dto::CATALOG_ONLY_GUARD_PLUGINS, &guard_names)
        .into_iter()
        .map(|name| crate::domain::gts_helpers::guard_plugin(&name))
        .collect::<Vec<_>>();
    let reserved_transform = reserved_ids(
        &crate::domain::dto::CATALOG_ONLY_TRANSFORM_PLUGINS,
        &transform_names,
    )
    .into_iter()
    .map(|name| crate::domain::gts_helpers::transform_plugin(&name))
    .collect::<Vec<_>>();

    auth.extend(reserved_auth.iter().cloned());
    guard.extend(reserved_guard.iter().cloned());
    transform.extend(reserved_transform.iter().cloned());

    let reserved = if reserved_auth.is_empty()
        && reserved_guard.is_empty()
        && reserved_transform.is_empty()
    {
        None
    } else {
        Some(crate::api::rest::dto::PluginCatalogReserved {
            auth: reserved_auth,
            guard: reserved_guard,
            transform: reserved_transform,
        })
    };

    axum::Json(PluginCatalog {
        auth,
        guard,
        transform,
        auth_names,
        guard_names,
        transform_names,
        reserved,
    })
}

/// The catalog-only names of one family that this build does not implement.
fn reserved_ids(catalog_only: &[&str], built_in: &[String]) -> Vec<String> {
    catalog_only
        .iter()
        .filter(|name| !built_in.iter().any(|known| known == *name))
        .map(|name| (*name).to_owned())
        .collect()
}

/// Short registry names for a list of GTS ids, in the same order.
fn names_of(ids: &[String]) -> Vec<String> {
    ids.iter()
        .map(|id| {
            crate::domain::dto::builtin_plugin_name(id)
                .unwrap_or_else(|| id.rsplit('.').next().unwrap_or(id).to_owned())
        })
        .collect()
}

/// `POST /oagw/v1/plugins`
pub async fn create(
    Extension(state): Extension<ApiState>,
    Tenant(tenant): Tenant,
    uri: Uri,
    body: Bytes,
) -> Result<axum::response::Response, ApiError> {
    let body: PluginRequestDto = parse_body(&body)?;
    let plugin = crate::domain::dto::Plugin {
        id: uuid::Uuid::nil(),
        tenant_id: tenant,
        created_at: 0,
        plugin_type: body.plugin_type,
        name: body.name,
        source: body.source,
        config: body.config,
    };
    let stored = state
        .plugins
        .create(tenant, plugin)
        .await
        .map_err(ApiError::from)?;
    let id = stored.id.to_string();
    let dto = PluginResponseDto::from(stored);
    Ok(toolkit::api::canonical_prelude::created_json(dto, &uri, &id).into_response())
}

/// `GET /oagw/v1/plugins/{id}/source`
///
/// Returns the stored source text of a tenant-defined plugin. A built-in name
/// (or a built-in GTS id) is not a stored plugin, so it answers `404` with the
/// same `cf.oagw.plugin.not_found.v1` problem as an unknown id.
pub async fn source(
    Extension(state): Extension<ApiState>,
    Tenant(tenant): Tenant,
    Path(id): Path<String>,
) -> Result<axum::Json<crate::api::rest::dto::PluginSourceDto>, ApiError> {
    let uuid = crate::api::rest::extractors::ResourceId::parse(&id).map_err(|_| {
        ApiError::from(DomainError::PluginNotFound)
    })?;
    let plugin = state.plugins.get(tenant, uuid.0).await.map_err(ApiError::from)?;
    Ok(axum::Json(crate::api::rest::dto::PluginSourceDto {
        id: plugin.id,
        plugin_type: plugin.plugin_type,
        name: plugin.name,
        source: plugin.source,
    }))
}

/// `GET /oagw/v1/plugins/{id}`
pub async fn get(
    Extension(state): Extension<ApiState>,
    Tenant(tenant): Tenant,
    Path(id): Path<ResourceId>,
) -> Result<axum::Json<PluginResponseDto>, ApiError> {
    let plugin = state.plugins.get(tenant, id.0).await.map_err(ApiError::from)?;
    Ok(axum::Json(PluginResponseDto::from(plugin)))
}

/// `DELETE /oagw/v1/plugins/{id}`
pub async fn delete(
    Extension(state): Extension<ApiState>,
    Tenant(tenant): Tenant,
    Path(id): Path<ResourceId>,
) -> Result<axum::http::StatusCode, ApiError> {
    state.plugins.delete(tenant, id.0).await.map_err(ApiError::from)?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

