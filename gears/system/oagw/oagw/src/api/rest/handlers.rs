//! Axum handlers for the OAGW REST surface.
//!
//! Management handlers translate `SecurityContext` + request DTOs into
//! `ControlPlane` calls and render the domain records directly as JSON
//! (records are the wire format — see `domain/model.rs`). The proxy
//! catch-all drives the data plane and short-circuits CORS preflights
//! per ADR 0004 (the gateway auth middleware hands anonymous preflights
//! through untouched).

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{OriginalUri, Path};
use axum::http::Method;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use toolkit::api::odata::OData;
use toolkit::api::response::{created_json, no_content};
use toolkit_odata::Page;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::dto;
use crate::domain::error::OagwError;
use crate::domain::model::{PluginCreateRequest, RouteRequest, RouteUpdateRequest, UpstreamRequest};
use crate::domain::services::ControlPlane;
use crate::infra::cors;
use crate::infra::proxy::DataPlane;

/// The gear-relative API prefix inside the full request path. The
/// api-gateway nests gear routers under its own (possibly empty)
/// `prefix_path`, so `OriginalUri` may or may not carry a leading
/// `/api` etc. — everything after the last `/oagw/v1` marker is the
/// gear-relative path.
const GEAR_PATH_MARKER: &str = "/oagw/v1";

// =====================================================================
//                            Upstreams
// =====================================================================

/// List upstreams with OData filter/select/orderby + cursor paging.
pub async fn list_upstreams(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlane>>,
    OData(query): OData,
) -> Result<Json<Page<serde_json::Value>>, OagwError> {
    let records = svc.list_upstreams(&ctx)?;
    let page = dto::page(
        records,
        &query,
        &dto::upstream_field_value,
        &|record| serde_json::to_value(record).unwrap_or(serde_json::Value::Null),
    );
    Ok(Json(page))
}

/// Create an upstream (201 + `Location`).
pub async fn create_upstream(
    OriginalUri(uri): OriginalUri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlane>>,
    Json(req): Json<UpstreamRequest>,
) -> Result<impl IntoResponse, OagwError> {
    let record = svc.create_upstream(&ctx, req).await?;
    let id = record.id.to_string();
    Ok(created_json(record, &uri, &id).into_response())
}

/// Get one upstream by id.
pub async fn get_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlane>>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, OagwError> {
    let record = svc.get_upstream(&ctx, id)?;
    Ok(Json(serde_json::to_value(record).unwrap_or(serde_json::Value::Null)))
}

/// Full replacement of an upstream (alias immutable once set).
pub async fn replace_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlane>>,
    Path(id): Path<Uuid>,
    Json(req): Json<UpstreamRequest>,
) -> Result<Json<serde_json::Value>, OagwError> {
    let record = svc.replace_upstream(&ctx, id, req)?;
    Ok(Json(serde_json::to_value(record).unwrap_or(serde_json::Value::Null)))
}

/// Delete an upstream (and its routes).
pub async fn delete_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlane>>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, OagwError> {
    svc.delete_upstream(&ctx, id)?;
    Ok(no_content().into_response())
}

// =====================================================================
//                              Routes
// =====================================================================

/// List routes with OData filter/select/orderby + cursor paging.
pub async fn list_routes(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlane>>,
    OData(query): OData,
) -> Result<Json<Page<serde_json::Value>>, OagwError> {
    let records = svc.list_routes(&ctx)?;
    let page = dto::page(
        records,
        &query,
        &dto::route_field_value,
        &|record| serde_json::to_value(record).unwrap_or(serde_json::Value::Null),
    );
    Ok(Json(page))
}

/// Create a route (201 + `Location`).
pub async fn create_route(
    OriginalUri(uri): OriginalUri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlane>>,
    Json(req): Json<RouteRequest>,
) -> Result<impl IntoResponse, OagwError> {
    let record = svc.create_route(&ctx, req).await?;
    let id = record.id.to_string();
    Ok(created_json(record, &uri, &id).into_response())
}

/// Get one route by id.
pub async fn get_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlane>>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, OagwError> {
    let record = svc.get_route(&ctx, id)?;
    Ok(Json(serde_json::to_value(record).unwrap_or(serde_json::Value::Null)))
}

/// Full replacement of a route (`upstream_id` immutable).
pub async fn replace_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlane>>,
    Path(id): Path<Uuid>,
    Json(req): Json<RouteUpdateRequest>,
) -> Result<Json<serde_json::Value>, OagwError> {
    let record = svc.replace_route(&ctx, id, req)?;
    Ok(Json(serde_json::to_value(record).unwrap_or(serde_json::Value::Null)))
}

/// Delete a route.
pub async fn delete_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlane>>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, OagwError> {
    svc.delete_route(&ctx, id)?;
    Ok(no_content().into_response())
}

// =====================================================================
//                             Plugins
// =====================================================================

/// List plugins with OData filter/select/orderby + cursor paging.
pub async fn list_plugins(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlane>>,
    OData(query): OData,
) -> Result<Json<Page<serde_json::Value>>, OagwError> {
    let records = svc.list_plugins(&ctx)?;
    let page = dto::page(
        records,
        &query,
        &dto::plugin_field_value,
        &|record| serde_json::to_value(record).unwrap_or(serde_json::Value::Null),
    );
    Ok(Json(page))
}

/// Create a plugin (201 + `Location`).
pub async fn create_plugin(
    OriginalUri(uri): OriginalUri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlane>>,
    Json(req): Json<PluginCreateRequest>,
) -> Result<impl IntoResponse, OagwError> {
    let record = svc.create_plugin(&ctx, req).await?;
    let id = record.id.to_string();
    Ok(created_json(record, &uri, &id).into_response())
}

/// Get one plugin by id.
pub async fn get_plugin(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlane>>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, OagwError> {
    let record = svc.get_plugin(&ctx, id)?;
    Ok(Json(serde_json::to_value(record).unwrap_or(serde_json::Value::Null)))
}

/// Get the Starlark source of a plugin.
pub async fn get_plugin_source(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlane>>,
    Path(id): Path<Uuid>,
) -> Result<Json<String>, OagwError> {
    let source = svc.get_plugin_source(&ctx, id)?;
    Ok(Json(source))
}

/// Delete a plugin (409 when still referenced).
pub async fn delete_plugin(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlane>>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, OagwError> {
    svc.delete_plugin(&ctx, id)?;
    Ok(no_content().into_response())
}

// =====================================================================
//                             Proxy
// =====================================================================

/// Proxy catch-all: `{METHOD} /oagw/v1/proxy/{alias}/{suffix}`.
///
/// CORS preflights (the gateway auth middleware already short-circuits
/// them) are answered permissively here per ADR 0004 before any
/// upstream resolution. Everything else is delegated to the data plane.
pub async fn proxy(
    Extension(ctx): Extension<SecurityContext>,
    Extension(dp): Extension<Arc<DataPlane>>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Result<Response, OagwError> {
    if cors::is_preflight(&method, &headers) {
        return Ok(cors::preflight_response(&headers));
    }
    let raw_path = gear_relative_path(uri.path());
    let query = uri.query();
    dp.proxy(&ctx, &method, &raw_path, query, headers, body).await
}

/// Strip everything before the gear-relative `/oagw/v1` marker so the
/// data plane always receives `/proxy/...` regardless of the gateway's
/// `prefix_path`.
fn gear_relative_path(path: &str) -> &str {
    match path.find(GEAR_PATH_MARKER) {
        Some(idx) => &path[idx + GEAR_PATH_MARKER.len()..],
        None => path,
    }
}
