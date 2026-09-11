// Updated: 2026-09-01 by Constructor Tech
//! Handlers for `/oagw/v1/plugins`.
//!
//! Custom plugins are immutable: they are created, read and deleted, and an
//! attempt to replace one is a conflict. `/plugins/{id}/source` returns the
//! Starlark declaration a custom plugin was created with.

use std::sync::Arc;

use axum::Extension;
use axum::Json;
use axum::extract::{Path, Query};
use toolkit::api::canonical_prelude::ApiResult;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::dto::{CreatePluginRequest, ListQuery, Page, PluginSourceView, PluginView};
use crate::domain::services::management::ManagementService;

/// List custom plugins visible to the caller.
pub async fn list(
    Extension(svc): Extension<Arc<ManagementService>>,
    Extension(ctx): Extension<SecurityContext>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<Page<PluginView>>> {
    let options = crate::api::rest::odata::ListOptions::from(query);
    let items = svc.list_plugins(&ctx, options.kind_filter()).await?;
    let page = Page::slice(&items, options.offset(), options.limit());
    Ok(Json(Page {
        items: page
            .items
            .into_iter()
            .map(|p| {
                let kind = p.kind;
                PluginView::new(p, kind)
            })
            .collect(),
        total: page.total,
        offset: page.offset,
        limit: page.limit,
    }))
}

/// Read one custom plugin.
pub async fn get(
    Extension(svc): Extension<Arc<ManagementService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<PluginView>> {
    let plugin = svc.get_plugin(&ctx, id).await?;
    let kind = plugin.kind;
    Ok(Json(PluginView::new(plugin, kind)))
}

/// Create a custom plugin.
pub async fn create(
    Extension(svc): Extension<Arc<ManagementService>>,
    Extension(ctx): Extension<SecurityContext>,
    axum::Json(payload): axum::Json<CreatePluginRequest>,
) -> ApiResult<(axum::http::StatusCode, Json<PluginView>)> {
    let created = svc.create_plugin(&ctx, payload.into()).await?;
    let kind = created.kind;
    Ok((
        axum::http::StatusCode::CREATED,
        Json(PluginView::new(created, kind)),
    ))
}

/// Custom plugins are immutable: always 409.
pub async fn replace(
    Extension(svc): Extension<Arc<ManagementService>>,
) -> ApiResult<Json<PluginView>> {
    let updated = svc.update_plugin()?;
    let kind = updated.kind;
    Ok(Json(PluginView::new(updated, kind)))
}

/// Delete an unreferenced plugin.
pub async fn delete(
    Extension(svc): Extension<Arc<ManagementService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
) -> ApiResult<axum::http::StatusCode> {
    svc.delete_plugin(&ctx, id).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// The Starlark declaration a custom plugin was created with.
pub async fn source(
    Extension(svc): Extension<Arc<ManagementService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<PluginSourceView>> {
    let source = svc.plugin_source(&ctx, id).await?;
    Ok(Json(PluginSourceView { source }))
}
