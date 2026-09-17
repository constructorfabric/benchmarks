//! REST handlers of the management API.
//!
//! Thin adapters over the [`Service`](crate::domain::service::Service): they
//! extract the caller's [`SecurityContext`], delegate, and shape the response.
//! Every failure is already a `CanonicalError`, so `?` is the whole error
//! mapping — the canonical error middleware renders it as an RFC 9457
//! `Problem`.
//!
//! The `SecurityContext` is read from the request extensions the gateway
//! middleware (or the test harness) inserts. A request without one is rejected
//! with 401, rather than with the 500 a missing-extension extractor would
//! produce.
//!
//! List handlers project the DTO through `apply_select`, so `$select` is
//! answered with exactly the requested fields.

use std::sync::Arc;

use axum::extract::{Extension, Path, Query};
use axum::response::IntoResponse;
use toolkit::api::canonical_prelude::*;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::dto::{
    ListParamsDto, RouteDto, RouteRequestDto, RouteStatusRequestDto, UpstreamDto,
    UpstreamRequestDto, UpstreamStatusRequestDto,
};
use crate::domain::service::{ListParams, ListQuery, Service};

/// The caller's tenant, or 401 when the request carries no security context.
fn caller(ctx: Option<Extension<SecurityContext>>) -> Result<SecurityContext, CanonicalError> {
    ctx.map(|Extension(ctx)| ctx).ok_or_else(|| {
        CanonicalError::unauthenticated()
            .with_reason("MISSING_SECURITY_CONTEXT")
            .create()
    })
}

// ---------------------------------------------------------------------------
// Upstreams
// ---------------------------------------------------------------------------

/// `POST /oagw/v1/upstreams`
///
/// Creates an upstream owned by the calling tenant and returns 201 with the
/// stored representation and a `Location` header.
///
/// # Errors
/// A canonical `Problem` for an invalid payload (400), an alias already in use
/// or enforced by an ancestor upstream (409), an unreadable tenant hierarchy
/// (503), or a missing security context (401).
pub async fn create_upstream(
    uri: axum::http::Uri,
    ctx: Option<Extension<SecurityContext>>,
    Extension(svc): Extension<Arc<Service>>,
    Json(body): Json<UpstreamRequestDto>,
) -> ApiResult<impl IntoResponse> {
    let ctx = caller(ctx)?;
    let created = svc.create_upstream(&ctx, body.into()).await?;
    let id = created.id.unwrap_or_default().to_string();
    Ok(created_json(UpstreamDto::from(created), &uri, &id))
}

/// `GET /oagw/v1/upstreams`
///
/// # Errors
/// A canonical `Problem` for an unsupported `$filter`, `$orderby` or `$top`
/// (400), or a missing security context (401).
pub async fn list_upstreams(
    ctx: Option<Extension<SecurityContext>>,
    Extension(svc): Extension<Arc<Service>>,
    Query(params): Query<ListParamsDto>,
) -> ApiResult<impl IntoResponse> {
    let ctx = caller(ctx)?;
    let query = ListQuery::parse(&ListParams::from(params))?;
    let items = svc
        .list_upstreams(ctx.subject_tenant_id(), &query)?
        .into_iter()
        .map(UpstreamDto::from)
        .collect::<Vec<_>>();
    let body: Vec<serde_json::Value> = items
        .into_iter()
        .map(|item| apply_select(item, query.select.as_deref()))
        .collect();
    Ok(ok_json(body))
}

/// `GET /oagw/v1/upstreams/{id}`
///
/// # Errors
/// A canonical `Problem` when the upstream does not belong to the calling
/// tenant (404), or when the security context is missing (401).
pub async fn get_upstream(
    ctx: Option<Extension<SecurityContext>>,
    Extension(svc): Extension<Arc<Service>>,
    Path(id): Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    let ctx = caller(ctx)?;
    let upstream = svc.get_upstream(ctx.subject_tenant_id(), id)?;
    Ok(ok_json(UpstreamDto::from(upstream)))
}

/// `PUT /oagw/v1/upstreams/{id}` — full replacement.
///
/// # Errors
/// A canonical `Problem` for an unknown id (404), an invalid payload or a
/// rejected alias transition (400), an enforced ancestor upstream (409), an
/// unreadable tenant hierarchy (503), or a missing security context (401).
pub async fn replace_upstream(
    ctx: Option<Extension<SecurityContext>>,
    Extension(svc): Extension<Arc<Service>>,
    Path(id): Path<Uuid>,
    Json(body): Json<UpstreamRequestDto>,
) -> ApiResult<impl IntoResponse> {
    let ctx = caller(ctx)?;
    let replaced = svc
        .replace_upstream(&ctx, ctx.subject_tenant_id(), id, body.into())
        .await?;
    Ok(ok_json(UpstreamDto::from(replaced)))
}

/// `DELETE /oagw/v1/upstreams/{id}` — deletes the upstream and its routes.
///
/// # Errors
/// A canonical `Problem` when the upstream does not belong to the calling
/// tenant (404), or when the security context is missing (401).
pub async fn delete_upstream(
    ctx: Option<Extension<SecurityContext>>,
    Extension(svc): Extension<Arc<Service>>,
    Path(id): Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    let ctx = caller(ctx)?;
    svc.delete_upstream(ctx.subject_tenant_id(), id)?;
    Ok(no_content())
}

/// `POST /oagw/v1/upstreams/{id}/status`
///
/// # Errors
/// A canonical `Problem` when the upstream does not belong to the calling
/// tenant (404), or when the security context is missing (401).
pub async fn set_upstream_status(
    ctx: Option<Extension<SecurityContext>>,
    Extension(svc): Extension<Arc<Service>>,
    Path(id): Path<Uuid>,
    Json(body): Json<UpstreamStatusRequestDto>,
) -> ApiResult<impl IntoResponse> {
    let ctx = caller(ctx)?;
    let updated = svc.set_upstream_enabled(ctx.subject_tenant_id(), id, body.enabled)?;
    Ok(ok_json(UpstreamDto::from(updated)))
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// `POST /oagw/v1/routes`
///
/// # Errors
/// A canonical `Problem` for an invalid payload (400), an `upstream_id`
/// outside the calling tenant (404), a match rule already in use (409), or a
/// missing security context (401).
pub async fn create_route(
    uri: axum::http::Uri,
    ctx: Option<Extension<SecurityContext>>,
    Extension(svc): Extension<Arc<Service>>,
    Json(body): Json<RouteRequestDto>,
) -> ApiResult<impl IntoResponse> {
    let ctx = caller(ctx)?;
    let created = svc.create_route(ctx.subject_tenant_id(), body.into())?;
    let id = created.id.unwrap_or_default().to_string();
    Ok(created_json(RouteDto::from(created), &uri, &id))
}

/// `GET /oagw/v1/routes`
///
/// # Errors
/// A canonical `Problem` for an unsupported `$filter`, `$orderby` or `$top`
/// (400), or a missing security context (401).
pub async fn list_routes(
    ctx: Option<Extension<SecurityContext>>,
    Extension(svc): Extension<Arc<Service>>,
    Query(params): Query<ListParamsDto>,
) -> ApiResult<impl IntoResponse> {
    let ctx = caller(ctx)?;
    let query = ListQuery::parse(&ListParams::from(params))?;
    let items = svc
        .list_routes(ctx.subject_tenant_id(), &query)?
        .into_iter()
        .map(RouteDto::from)
        .collect::<Vec<_>>();
    let body: Vec<serde_json::Value> = items
        .into_iter()
        .map(|item| apply_select(item, query.select.as_deref()))
        .collect();
    Ok(ok_json(body))
}

/// `GET /oagw/v1/routes/{id}`
///
/// # Errors
/// A canonical `Problem` when the route does not belong to the calling tenant
/// (404), or when the security context is missing (401).
pub async fn get_route(
    ctx: Option<Extension<SecurityContext>>,
    Extension(svc): Extension<Arc<Service>>,
    Path(id): Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    let ctx = caller(ctx)?;
    let route = svc.get_route(ctx.subject_tenant_id(), id)?;
    Ok(ok_json(RouteDto::from(route)))
}

/// `PUT /oagw/v1/routes/{id}` — full replacement; `upstream_id` is immutable.
///
/// # Errors
/// A canonical `Problem` for an unknown id (404), an invalid payload or an
/// attempted `upstream_id` change (400), a match rule already in use (409), or
/// a missing security context (401).
pub async fn replace_route(
    ctx: Option<Extension<SecurityContext>>,
    Extension(svc): Extension<Arc<Service>>,
    Path(id): Path<Uuid>,
    Json(body): Json<RouteRequestDto>,
) -> ApiResult<impl IntoResponse> {
    let ctx = caller(ctx)?;
    let replaced = svc.replace_route(ctx.subject_tenant_id(), id, body.into())?;
    Ok(ok_json(RouteDto::from(replaced)))
}

/// `DELETE /oagw/v1/routes/{id}`
///
/// # Errors
/// A canonical `Problem` when the route does not belong to the calling tenant
/// (404), or when the security context is missing (401).
pub async fn delete_route(
    ctx: Option<Extension<SecurityContext>>,
    Extension(svc): Extension<Arc<Service>>,
    Path(id): Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    let ctx = caller(ctx)?;
    svc.delete_route(ctx.subject_tenant_id(), id)?;
    Ok(no_content())
}

/// `POST /oagw/v1/routes/{id}/status`
///
/// # Errors
/// A canonical `Problem` when the route does not belong to the calling tenant
/// (404), or when the security context is missing (401).
pub async fn set_route_status(
    ctx: Option<Extension<SecurityContext>>,
    Extension(svc): Extension<Arc<Service>>,
    Path(id): Path<Uuid>,
    Json(body): Json<RouteStatusRequestDto>,
) -> ApiResult<impl IntoResponse> {
    let ctx = caller(ctx)?;
    let updated = svc.set_route_enabled(ctx.subject_tenant_id(), id, body.enabled)?;
    Ok(ok_json(RouteDto::from(updated)))
}
