//! Plugin handlers of the OAGW management REST surface.

use axum::Extension;
use axum::extract::Path;
use axum::http::HeaderMap;
use axum::http::Uri;
use axum::response::IntoResponse;

use super::{Service, path_uuid};
use crate::api::rest::dto::{CreatePluginRequest, PluginDto, PluginSourceDto};
use crate::api::rest::extractors::{JsonBody, PluginList};
use crate::domain::error::{ApiResult, OagwError};
use crate::domain::model::parse_plugin_id;
use toolkit::api::canonical_prelude::{created_json, no_content, ok_json};
use toolkit_security::SecurityContext;

/// The request id of a mutation, from the `x-request-id` header.
fn request_id(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
}

/// Missing plugin problem document for `id`.
fn missing(id: uuid::Uuid) -> OagwError {
    OagwError::not_found(format!(
        "plugin gts.cf.core.oagw.plugin.v1~{id} does not exist"
    ))
    .with_plugin_id(format!("gts.cf.core.oagw.plugin.v1~{id}"))
}

/// `POST /oagw/v1/plugins` — registers a plugin for the calling tenant.
///
/// Plugins are immutable: there is no `PUT`, and a re-registration of the same
/// id is a `409` (DESIGN section 3.3).
///
/// # Errors
///
/// Returns the problem document reported by
/// [`ControlPlaneService::create_plugin`](crate::domain::services::ControlPlaneService::create_plugin).
pub async fn create_plugin(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Service>,
    headers: HeaderMap,
    JsonBody(request): JsonBody<CreatePluginRequest>,
) -> ApiResult<impl IntoResponse> {
    let input = request.as_input();
    let created = svc.create_plugin(&ctx, request_id(&headers), &input)?;
    Ok(created_json(
        PluginDto::from(&*created),
        &uri,
        &created.id.to_string(),
    ))
}

/// `GET /oagw/v1/plugins` — lists the plugins of the calling tenant.
///
/// # Errors
///
/// Returns the problem document reported by the OData query parser.
pub async fn list_plugins(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Service>,
    PluginList(query): PluginList,
) -> ApiResult<impl IntoResponse> {
    let items = svc
        .list_plugins(&ctx)
        .into_iter()
        .map(|plugin| PluginDto::from(&*plugin))
        .collect();
    let page = query.apply_values(items)?;
    Ok(ok_json(page))
}

/// `GET /oagw/v1/plugins/{id}` — reads one plugin of the calling tenant.
///
/// # Errors
///
/// Returns [`OagwError::NotFound`] when the calling tenant does not own the
/// plugin.
pub async fn get_plugin(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Service>,
    Path(raw_id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let id = path_uuid(&raw_id, "plugin", parse_plugin_id)?;
    let plugin = svc.get_plugin(&ctx, id).ok_or_else(|| missing(id))?;
    Ok(ok_json(PluginDto::from(&*plugin)))
}

/// `GET /oagw/v1/plugins/{id}/source` — returns the deterministic plugin
/// definition rendered for inspection and review.
///
/// # Errors
///
/// Returns [`OagwError::NotFound`] when the calling tenant does not own the
/// plugin.
pub async fn get_plugin_source(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Service>,
    Path(raw_id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let id = path_uuid(&raw_id, "plugin", parse_plugin_id)?;
    let (plugin, source) = svc.plugin_source(&ctx, id)?;
    let rendered = PluginSourceDto {
        plugin_id: plugin.id,
        plugin_type: plugin.plugin_type.clone(),
        source,
    };
    Ok(ok_json(rendered))
}

/// `DELETE /oagw/v1/plugins/{id}` — deletes a plugin of the calling tenant.
///
/// # Errors
///
/// Returns [`OagwError::PluginInUse`] with `plugin_id` and `referenced_by`
/// when an upstream or a route still references the plugin (ADR-0001), and
/// [`OagwError::NotFound`] when the calling tenant does not own it.
pub async fn delete_plugin(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Service>,
    Path(raw_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<impl IntoResponse> {
    let id = path_uuid(&raw_id, "plugin", parse_plugin_id)?;
    svc.delete_plugin(&ctx, request_id(&headers), id)?;
    Ok(no_content())
}
