// Created: 2026-08-29 by Constructor Tech
//! Management REST handlers: upstreams, routes and custom plugins.

use std::sync::Arc;

use axum::Extension;
use axum::Json;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::IntoResponse;
use serde_json::Value;
use uuid::Uuid;

use crate::api::rest::dto::{ListQuery, PluginDto, RouteDto, UpstreamDto};
use crate::api::rest::error::ApiError;
use crate::api::rest::odata;
use crate::domain::error::OagwError;
use crate::domain::model::{PluginCreate, RouteCreate, UpstreamCreate};
use crate::domain::services::management::ControlPlaneService;

use super::{CtxExtension, Services, tenant_of};

type HandlerResult<T> = Result<T, ApiError>;

fn to_api(error: OagwError) -> ApiError {
    ApiError::new(error)
}

fn not_found(what: &str, id: Uuid) -> ApiError {
    ApiError::new(OagwError::RouteNotFound(format!("{what} '{id}' not found")))
}

fn parse_body<T, E: std::fmt::Display>(result: Result<T, E>) -> Result<T, ApiError> {
    result.map_err(|error| ApiError::new(OagwError::Validation(error.to_string())))
}

fn serialize_page<T: serde::Serialize>(
    dtos: Vec<T>,
    query: &ListQuery,
) -> Result<toolkit::Page<Value>, ApiError> {
    let items: Vec<Value> = dtos
        .iter()
        .map(|dto| serde_json::to_value(dto).unwrap_or_default())
        .collect();
    let (page, _total) = odata::apply_query_page(items, query).map_err(ApiError::new)?;
    let page_size = u64::try_from(query.page_size()).unwrap_or_default();
    Ok(toolkit::Page::new(
        page,
        toolkit::PageInfo {
            next_cursor: None,
            prev_cursor: None,
            limit: page_size,
        },
    ))
}

// ---------------------------------------------------------------------------------------
// Upstreams
// ---------------------------------------------------------------------------------------

/// `POST /oagw/v1/upstreams` — create an upstream, 201 + `Location`.
pub async fn create_upstream(
    uri: Uri,
    Extension(services): Extension<Arc<Services>>,
    ctx: CtxExtension,
    body: Result<axum::Json<UpstreamCreate>, axum::extract::rejection::JsonRejection>,
) -> HandlerResult<impl IntoResponse> {
    let spec = parse_body(body.map(|Json(value)| value))?;
    let created = services
        .control_plane
        .create_upstream(tenant_of(&ctx), spec)
        .map_err(to_api)?;
    let location = format!("{}/{}", uri.path().trim_end_matches('/'), created.id);
    Ok((
        StatusCode::CREATED,
        [("location", location)],
        axum::Json(UpstreamDto::from(created)),
    ))
}

/// `GET /oagw/v1/upstreams` — list with OData paging.
pub async fn list_upstreams(
    Extension(services): Extension<Arc<Services>>,
    ctx: CtxExtension,
    Query(query): Query<ListQuery>,
) -> HandlerResult<impl IntoResponse> {
    let dtos: Vec<UpstreamDto> = services
        .control_plane
        .list_upstreams(tenant_of(&ctx))
        .into_iter()
        .map(Into::into)
        .collect();
    Ok(axum::Json(serialize_page(dtos, &query)?))
}

/// `GET /oagw/v1/upstreams/{id}` — 200 / 404.
pub async fn get_upstream(
    Extension(services): Extension<Arc<Services>>,
    ctx: CtxExtension,
    Path(id): Path<Uuid>,
) -> HandlerResult<impl IntoResponse> {
    let upstream = services
        .control_plane
        .get_upstream(tenant_of(&ctx), id)
        .ok_or_else(|| not_found("upstream", id))?;
    Ok(axum::Json(UpstreamDto::from(upstream)))
}

/// `PUT /oagw/v1/upstreams/{id}` — full replace, alias immutable.
pub async fn replace_upstream(
    Extension(services): Extension<Arc<Services>>,
    ctx: CtxExtension,
    Path(id): Path<Uuid>,
    body: Result<axum::Json<UpstreamCreate>, axum::extract::rejection::JsonRejection>,
) -> HandlerResult<impl IntoResponse> {
    let spec = parse_body(body.map(|Json(value)| value))?;
    let updated = services
        .control_plane
        .replace_upstream(tenant_of(&ctx), id, spec)
        .map_err(to_api)?;
    Ok(axum::Json(UpstreamDto::from(updated)))
}

/// `DELETE /oagw/v1/upstreams/{id}` — 204 / 404.
pub async fn delete_upstream(
    Extension(services): Extension<Arc<Services>>,
    ctx: CtxExtension,
    Path(id): Path<Uuid>,
) -> HandlerResult<impl IntoResponse> {
    services
        .control_plane
        .delete_upstream(tenant_of(&ctx), id)
        .map_err(to_api)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /oagw/v1/upstreams/{id}/enable`
pub async fn enable_upstream(
    services: Extension<Arc<Services>>,
    ctx: CtxExtension,
    Path(id): Path<Uuid>,
) -> HandlerResult<impl IntoResponse> {
    set_enabled(services, ctx, id, true).await
}

/// `POST /oagw/v1/upstreams/{id}/disable`
pub async fn disable_upstream(
    services: Extension<Arc<Services>>,
    ctx: CtxExtension,
    Path(id): Path<Uuid>,
) -> HandlerResult<impl IntoResponse> {
    set_enabled(services, ctx, id, false).await
}

async fn set_enabled(
    services: Extension<Arc<Services>>,
    ctx: CtxExtension,
    id: Uuid,
    enabled: bool,
) -> HandlerResult<impl IntoResponse> {
    let upstream = services
        .control_plane
        .set_upstream_enabled(tenant_of(&ctx), id, enabled)
        .map_err(to_api)?;
    Ok(axum::Json(UpstreamDto::from(upstream)))
}

// ---------------------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------------------

/// `POST /oagw/v1/routes` — validates `upstream_id`.
pub async fn create_route(
    Extension(services): Extension<Arc<Services>>,
    ctx: CtxExtension,
    body: Result<axum::Json<RouteCreate>, axum::extract::rejection::JsonRejection>,
) -> HandlerResult<impl IntoResponse> {
    let spec = parse_body(body.map(|Json(value)| value))?;
    let created = services
        .control_plane
        .create_route(tenant_of(&ctx), spec)
        .map_err(to_api)?;
    Ok((StatusCode::CREATED, axum::Json(RouteDto::from(created))))
}

/// `GET /oagw/v1/routes` — list with OData paging.
pub async fn list_routes(
    Extension(services): Extension<Arc<Services>>,
    ctx: CtxExtension,
    Query(query): Query<ListQuery>,
) -> HandlerResult<impl IntoResponse> {
    let dtos: Vec<RouteDto> = services
        .control_plane
        .list_routes(tenant_of(&ctx))
        .into_iter()
        .map(Into::into)
        .collect();
    Ok(axum::Json(serialize_page(dtos, &query)?))
}

/// `GET /oagw/v1/routes/{id}` — 200 / 404.
pub async fn get_route(
    Extension(services): Extension<Arc<Services>>,
    ctx: CtxExtension,
    Path(id): Path<Uuid>,
) -> HandlerResult<impl IntoResponse> {
    let route = services
        .control_plane
        .get_route(tenant_of(&ctx), id)
        .ok_or_else(|| not_found("route", id))?;
    Ok(axum::Json(RouteDto::from(route)))
}

/// `PUT /oagw/v1/routes/{id}` — full replace, `upstream_id` immutable.
pub async fn replace_route(
    Extension(services): Extension<Arc<Services>>,
    ctx: CtxExtension,
    Path(id): Path<Uuid>,
    body: Result<axum::Json<RouteCreate>, axum::extract::rejection::JsonRejection>,
) -> HandlerResult<impl IntoResponse> {
    let spec = parse_body(body.map(|Json(value)| value))?;
    let updated = services
        .control_plane
        .replace_route(tenant_of(&ctx), id, spec)
        .map_err(to_api)?;
    Ok(axum::Json(RouteDto::from(updated)))
}

/// `DELETE /oagw/v1/routes/{id}` — 204 / 404.
pub async fn delete_route(
    Extension(services): Extension<Arc<Services>>,
    ctx: CtxExtension,
    Path(id): Path<Uuid>,
) -> HandlerResult<impl IntoResponse> {
    services
        .control_plane
        .delete_route(tenant_of(&ctx), id)
        .map_err(to_api)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------------------

/// `POST /oagw/v1/plugins` — register a custom plugin.
pub async fn create_plugin(
    Extension(services): Extension<Arc<Services>>,
    ctx: CtxExtension,
    body: Result<axum::Json<PluginCreate>, axum::extract::rejection::JsonRejection>,
) -> HandlerResult<impl IntoResponse> {
    let spec = parse_body(body.map(|Json(value)| value))?;
    let created = services
        .control_plane
        .create_plugin(tenant_of(&ctx), spec)
        .map_err(to_api)?;
    Ok((StatusCode::CREATED, axum::Json(PluginDto::from(created))))
}

/// `GET /oagw/v1/plugins` — list with OData paging.
pub async fn list_plugins(
    Extension(services): Extension<Arc<Services>>,
    ctx: CtxExtension,
    Query(query): Query<ListQuery>,
) -> HandlerResult<impl IntoResponse> {
    let dtos: Vec<PluginDto> = services
        .control_plane
        .list_plugins(tenant_of(&ctx))
        .into_iter()
        .map(Into::into)
        .collect();
    Ok(axum::Json(serialize_page(dtos, &query)?))
}

/// `GET /oagw/v1/plugins/{id}` — 200 / 404.
pub async fn get_plugin(
    Extension(services): Extension<Arc<Services>>,
    ctx: CtxExtension,
    Path(id): Path<Uuid>,
) -> HandlerResult<impl IntoResponse> {
    let plugin = services
        .control_plane
        .get_plugin(tenant_of(&ctx), id)
        .ok_or_else(|| not_found("plugin", id))?;
    Ok(axum::Json(PluginDto::from(plugin)))
}

/// `DELETE /oagw/v1/plugins/{id}` — 204, or 409 with `referenced_by`.
pub async fn delete_plugin(
    Extension(services): Extension<Arc<Services>>,
    ctx: CtxExtension,
    Path(id): Path<Uuid>,
) -> HandlerResult<impl IntoResponse> {
    services
        .control_plane
        .delete_plugin(tenant_of(&ctx), id)
        .map_err(to_api)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /oagw/v1/plugins/{id}/source` — Starlark source as `text/plain`.
pub async fn get_plugin_source(
    Extension(services): Extension<Arc<Services>>,
    ctx: CtxExtension,
    Path(id): Path<Uuid>,
) -> HandlerResult<impl IntoResponse> {
    let plugin = services
        .control_plane
        .get_plugin(tenant_of(&ctx), id)
        .ok_or_else(|| not_found("plugin", id))?;
    Ok((
        [("content-type", "text/plain; charset=utf-8")],
        plugin.source_code,
    ))
}

/// Build the services bundle used by the handlers.
#[must_use]
pub fn bundle(
    control_plane: Arc<ControlPlaneService>,
    data_plane: Arc<crate::infra::proxy::service::DataPlaneService>,
) -> Arc<Services> {
    Arc::new(Services {
        control_plane,
        data_plane,
    })
}
