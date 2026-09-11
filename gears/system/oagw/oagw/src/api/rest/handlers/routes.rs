// Updated: 2026-09-01 by Constructor Tech
//! Handlers for `/oagw/v1/routes`.

use std::sync::Arc;

use axum::Extension;
use axum::Json;
use axum::extract::{Path, Query};
use toolkit::api::canonical_prelude::ApiResult;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::dto::{ListQuery, Page, RouteView};
use crate::domain::dto::Route;
use crate::domain::services::management::ManagementService;

/// List routes visible to the caller.
pub async fn list(
    Extension(svc): Extension<Arc<ManagementService>>,
    Extension(ctx): Extension<SecurityContext>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<Page<RouteView>>> {
    let options = crate::api::rest::odata::ListOptions::from(query);
    let mut items = svc.list_routes(&ctx).await?;
    if let Some(tag) = options.tag_filter() {
        items.retain(|r| r.tags.iter().any(|t| t == &tag));
    }
    items.sort_by_key(route_sort_key);
    if options.descending() {
        items.reverse();
    }
    let page = Page::slice(&items, options.offset(), options.limit());
    Ok(Json(Page {
        items: page.items.into_iter().map(RouteView::new).collect(),
        total: page.total,
        offset: page.offset,
        limit: page.limit,
    }))
}

/// Routes sort by their upstream, then by priority.
fn route_sort_key(route: &Route) -> (uuid::Uuid, i64) {
    (route.upstream_id, route.priority)
}

/// Read one route.
pub async fn get(
    Extension(svc): Extension<Arc<ManagementService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<RouteView>> {
    let route = svc.get_route(&ctx, id).await?;
    Ok(Json(RouteView::new(route)))
}

/// Create a route.
pub async fn create(
    Extension(svc): Extension<Arc<ManagementService>>,
    Extension(ctx): Extension<SecurityContext>,
    axum::Json(payload): axum::Json<Route>,
) -> ApiResult<(axum::http::StatusCode, Json<RouteView>)> {
    let created = svc.create_route(&ctx, payload).await?;
    Ok((
        axum::http::StatusCode::CREATED,
        Json(RouteView::new(created)),
    ))
}

/// Replace a route.
pub async fn replace(
    Extension(svc): Extension<Arc<ManagementService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
    axum::Json(payload): axum::Json<Route>,
) -> ApiResult<Json<RouteView>> {
    let updated = svc.replace_route(&ctx, id, payload).await?;
    Ok(Json(RouteView::new(updated)))
}

/// Delete a route.
pub async fn delete(
    Extension(svc): Extension<Arc<ManagementService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
) -> ApiResult<axum::http::StatusCode> {
    svc.delete_route(&ctx, id).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}
