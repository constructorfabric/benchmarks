// Created: 2026-08-31 by Constructor Tech
//! Custom plugin handlers (DESIGN §3.3, ADR-0002).
//!
//! Plugins are immutable: no PUT. `GET /plugins/{id}/source` returns the
//! Starlark source as `text/plain`.

use std::sync::Arc;

use axum::Extension;
use axum::extract::{Path, Query};
use axum::http::{Uri, header};
use axum::response::IntoResponse;
use toolkit::api::response::created_json;
use toolkit_security::SecurityContext;
use tracing::debug;

use crate::api::dto::{CreatePluginRequest, PluginDto};
use crate::api::extract::{JsonBody, parse_resource_id};
use crate::api::handlers::{deleted, serialize_all, to_page_json};
use crate::api::query::ListQuery;
use crate::domain::service::OagwService;
use crate::error::OagwResult;

/// List plugins with the documented `OData` subset.
///
/// # Errors
/// 400 on invalid query options, propagated from the store otherwise.
pub async fn list_plugins(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<OagwService>>,
    Query(pairs): Query<Vec<(String, String)>>,
) -> OagwResult<axum::Json<toolkit::Page<serde_json::Value>>> {
    let query = ListQuery::parse(&pairs)?;
    let items = serialize_all(svc.list_plugins(ctx.subject_tenant_id())?, PluginDto::from)?;
    let page = to_page_json(items, &query);
    debug!(tenant = %ctx.subject_tenant_id(), "listed plugins");
    Ok(page)
}

/// Create a custom plugin.
///
/// # Errors
/// 400 on missing required members, 409 on a name conflict.
pub async fn create_plugin(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<OagwService>>,
    JsonBody(request): JsonBody<CreatePluginRequest>,
) -> OagwResult<impl IntoResponse> {
    let tenant_id = ctx.subject_tenant_id();
    let record = svc.create_plugin(tenant_id, &request.into())?;
    let id = record.id.to_string();
    debug!(tenant = %tenant_id, plugin = %record.name, "plugin created");
    let dto = PluginDto::from(record);
    Ok(created_json(dto, &uri, &id))
}

/// Read one plugin.
///
/// # Errors
/// 400 on an unparseable id, 404 when the record is foreign or missing.
pub async fn get_plugin(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<OagwService>>,
    Path(id): Path<String>,
) -> OagwResult<axum::Json<PluginDto>> {
    let id = parse_resource_id(&id)?;
    let record = svc.get_plugin(ctx.subject_tenant_id(), id)?;
    Ok(axum::Json(PluginDto::from(record)))
}

/// Starlark source of one plugin.
///
/// # Errors
/// 400 on an unparseable id, 404 when the record is foreign or missing.
pub async fn get_plugin_source(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<OagwService>>,
    Path(id): Path<String>,
) -> OagwResult<impl IntoResponse> {
    let id = parse_resource_id(&id)?;
    let source = svc.plugin_source(ctx.subject_tenant_id(), id)?;
    debug!(id = %id, bytes = source.len(), "plugin source fetched");
    Ok((
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        source,
    ))
}

/// Delete a plugin; refuses while an upstream or route still binds it.
///
/// # Errors
/// 400 on an unparseable id, 404 on a foreign record, 409 `plugin.in_use`.
pub async fn delete_plugin(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<OagwService>>,
    Path(id): Path<String>,
) -> OagwResult<impl IntoResponse> {
    let id = parse_resource_id(&id)?;
    svc.delete_plugin(ctx.subject_tenant_id(), id)?;
    Ok(deleted())
}
