// Created: 2026-08-31 by Constructor Tech
//! Route management handlers (DESIGN §3.3).

use std::sync::Arc;

use axum::Extension;
use axum::extract::{Path, Query};
use axum::http::Uri;
use axum::response::IntoResponse;
use toolkit::api::response::created_json;
use toolkit_security::SecurityContext;
use tracing::debug;

use crate::api::dto::{CreateRouteRequest, RouteDto, UpdateRouteRequest};
use crate::api::extract::{JsonBody, parse_resource_id};
use crate::api::handlers::{deleted, serialize_all, to_page_json};
use crate::api::query::ListQuery;
use crate::domain::service::OagwService;
use crate::error::OagwResult;

/// List routes with the documented `OData` subset.
///
/// # Errors
/// 400 on invalid query options, propagated from the store otherwise.
pub async fn list_routes(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<OagwService>>,
    Query(pairs): Query<Vec<(String, String)>>,
) -> OagwResult<axum::Json<toolkit::Page<serde_json::Value>>> {
    let query = ListQuery::parse(&pairs)?;
    let items = serialize_all(svc.list_routes(ctx.subject_tenant_id())?, RouteDto::from)?;
    let page = to_page_json(items, &query);
    debug!(tenant = %ctx.subject_tenant_id(), "listed routes");
    Ok(page)
}

/// Create a route bound to an upstream of the calling tenant.
///
/// # Errors
/// 400 on an invalid match rule, 404 on a foreign upstream, 409 on a
/// duplicate match rule.
pub async fn create_route(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<OagwService>>,
    JsonBody(request): JsonBody<CreateRouteRequest>,
) -> OagwResult<impl IntoResponse> {
    let tenant_id = ctx.subject_tenant_id();
    let record = svc.create_route(tenant_id, &request.into())?;
    let id = record.id.to_string();
    debug!(tenant = %tenant_id, upstream = %record.upstream_id, "route created");
    let dto = RouteDto::from(record);
    Ok(created_json(dto, &uri, &id))
}

/// Read one route.
///
/// # Errors
/// 400 on an unparseable id, 404 when the record is foreign or missing.
pub async fn get_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<OagwService>>,
    Path(id): Path<String>,
) -> OagwResult<axum::Json<RouteDto>> {
    let id = parse_resource_id(&id)?;
    let record = svc.get_route(ctx.subject_tenant_id(), id)?;
    Ok(axum::Json(RouteDto::from(record)))
}

/// Replace a route in full; `upstream_id` is immutable.
///
/// # Errors
/// 400 on an invalid match rule, 404 on a foreign record, 409 on a duplicate
/// match rule.
pub async fn update_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<OagwService>>,
    Path(id): Path<String>,
    JsonBody(request): JsonBody<UpdateRouteRequest>,
) -> OagwResult<axum::Json<RouteDto>> {
    let id = parse_resource_id(&id)?;
    let record = svc.replace_route(ctx.subject_tenant_id(), id, &request.into())?;
    debug!(id = %id, "route replaced");
    Ok(axum::Json(RouteDto::from(record)))
}

/// Delete a route.
///
/// # Errors
/// 400 on an unparseable id, 404 when the record is foreign or missing.
pub async fn delete_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<OagwService>>,
    Path(id): Path<String>,
) -> OagwResult<impl IntoResponse> {
    let id = parse_resource_id(&id)?;
    svc.delete_route(ctx.subject_tenant_id(), id)?;
    Ok(deleted())
}
