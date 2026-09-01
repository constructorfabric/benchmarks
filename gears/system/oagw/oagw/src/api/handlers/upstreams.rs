// Created: 2026-08-31 by Constructor Tech
//! Upstream management handlers (DESIGN §3.3).
//!
//! Every handler is tenant-scoped through the `SecurityContext` extension and
//! returns OAGW problem+json on failure.

use std::sync::Arc;

use axum::Extension;
use axum::extract::{Path, Query};
use axum::http::Uri;
use axum::response::IntoResponse;
use toolkit::api::response::{created_json, no_content};
use toolkit_security::SecurityContext;
use tracing::debug;

use crate::api::dto::{CreateUpstreamRequest, UpdateUpstreamRequest, UpstreamDto};
use crate::api::extract::{JsonBody, parse_resource_id};
use crate::api::handlers::{serialize_all, to_page_json};
use crate::api::query::ListQuery;
use crate::domain::service::OagwService;
use crate::error::OagwResult;

/// List upstreams with the documented `OData` subset.
///
/// # Errors
/// 400 on invalid query options, propagated from the store otherwise.
pub async fn list_upstreams(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<OagwService>>,
    Query(pairs): Query<Vec<(String, String)>>,
) -> OagwResult<axum::Json<toolkit::Page<serde_json::Value>>> {
    let query = ListQuery::parse(&pairs)?;
    let items = serialize_all(
        svc.list_upstreams(ctx.subject_tenant_id())?,
        UpstreamDto::from,
    )?;
    let page = to_page_json(items, &query);
    debug!(tenant = %ctx.subject_tenant_id(), "listed upstreams");
    Ok(page)
}

/// Create an upstream; the alias is derived when omitted.
///
/// # Errors
/// 400 on invalid endpoints or a rejected alias, 409 on alias conflict.
pub async fn create_upstream(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<OagwService>>,
    JsonBody(request): JsonBody<CreateUpstreamRequest>,
) -> OagwResult<impl IntoResponse> {
    let tenant_id = ctx.subject_tenant_id();
    let record = svc.create_upstream(tenant_id, &request.into())?;
    let id = record.id.to_string();
    debug!(tenant = %tenant_id, alias = %record.alias, "upstream created");
    let dto = UpstreamDto::from(record);
    Ok(created_json(dto, &uri, &id))
}

/// Read one upstream.
///
/// # Errors
/// 400 on an unparseable id, 404 when the record is foreign or missing.
pub async fn get_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<OagwService>>,
    Path(id): Path<String>,
) -> OagwResult<axum::Json<UpstreamDto>> {
    let id = parse_resource_id(&id)?;
    let record = svc.get_upstream(ctx.subject_tenant_id(), id)?;
    Ok(axum::Json(UpstreamDto::from(record)))
}

/// Replace an upstream in full.
///
/// # Errors
/// 400 on invalid endpoints or an alias change, 404 on a foreign record,
/// 409 on alias conflict.
pub async fn update_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<OagwService>>,
    Path(id): Path<String>,
    JsonBody(request): JsonBody<UpdateUpstreamRequest>,
) -> OagwResult<axum::Json<UpstreamDto>> {
    let id = parse_resource_id(&id)?;
    let record = svc.replace_upstream(ctx.subject_tenant_id(), id, &request.into())?;
    debug!(id = %id, alias = %record.alias, "upstream replaced");
    Ok(axum::Json(UpstreamDto::from(record)))
}

/// Delete an upstream together with its routes.
///
/// # Errors
/// 400 on an unparseable id, 404 when the record is foreign or missing.
pub async fn delete_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<OagwService>>,
    Path(id): Path<String>,
) -> OagwResult<impl IntoResponse> {
    let id = parse_resource_id(&id)?;
    svc.delete_upstream(ctx.subject_tenant_id(), id)?;
    Ok(no_content())
}
