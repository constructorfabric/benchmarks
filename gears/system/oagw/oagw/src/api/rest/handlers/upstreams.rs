// Updated: 2026-09-01 by Constructor Tech
//! Handlers for `/oagw/v1/upstreams`.

use std::sync::Arc;

use axum::Extension;
use axum::Json;
use axum::extract::{Path, Query};
use toolkit::api::canonical_prelude::ApiResult;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::dto::{ListQuery, Page, UpstreamView};
use crate::domain::dto::Upstream;
use crate::domain::services::management::ManagementService;

/// List upstreams visible to the caller.
pub async fn list(
    Extension(svc): Extension<Arc<ManagementService>>,
    Extension(ctx): Extension<SecurityContext>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<Page<UpstreamView>>> {
    let options = crate::api::rest::odata::ListOptions::from(query);
    let mut items = svc.list_upstreams(&ctx).await?;
    if let Some(tag) = options.tag_filter() {
        items.retain(|u| u.tags.iter().any(|t| t == &tag));
    }
    items.sort_by_key(upstream_sort_key);
    if options.descending() {
        items.reverse();
    }
    let page = Page::slice(&items, options.offset(), options.limit());
    Ok(Json(Page {
        items: page.items.into_iter().map(UpstreamView::new).collect(),
        total: page.total,
        offset: page.offset,
        limit: page.limit,
    }))
}

/// Upstreams sort by alias, `None` sorting first.
fn upstream_sort_key(u: &Upstream) -> String {
    u.alias.clone().unwrap_or_default()
}

/// Read one upstream.
pub async fn get(
    Extension(svc): Extension<Arc<ManagementService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<UpstreamView>> {
    let upstream = svc.get_upstream(&ctx, id).await?;
    Ok(Json(UpstreamView::new(upstream)))
}

/// Create an upstream.
pub async fn create(
    Extension(svc): Extension<Arc<ManagementService>>,
    Extension(ctx): Extension<SecurityContext>,
    axum::Json(payload): axum::Json<Upstream>,
) -> ApiResult<(axum::http::StatusCode, Json<UpstreamView>)> {
    let created = svc.create_upstream(&ctx, payload).await?;
    Ok((
        axum::http::StatusCode::CREATED,
        Json(UpstreamView::new(created)),
    ))
}

/// Replace an upstream.
pub async fn replace(
    Extension(svc): Extension<Arc<ManagementService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
    axum::Json(payload): axum::Json<Upstream>,
) -> ApiResult<Json<UpstreamView>> {
    let updated = svc.replace_upstream(&ctx, id, payload).await?;
    Ok(Json(UpstreamView::new(updated)))
}

/// Delete an upstream, cascading its routes.
pub async fn delete(
    Extension(svc): Extension<Arc<ManagementService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
) -> ApiResult<axum::http::StatusCode> {
    svc.delete_upstream(&ctx, id).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}
