//! Management-plane handlers (`contracts/management-api.md`).

use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, Path};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use uuid::Uuid;

use crate::api::rest::dto::ListQuery;
use crate::api::rest::error::OagwError;
use crate::api::rest::extract::Body;
use crate::domain::error::DomainError;
use crate::domain::model::{Plugin, Route, Upstream};
use crate::domain::services::ManagementService;

/// `POST /oagw/v1/upstreams`
///
/// Creates an upstream; the alias is derived or checked before validation.
///
/// # Errors
/// [`OagwError`] for the documented `400`/`404`/`409` cases.
pub async fn create_upstream(
    Extension(service): Extension<Arc<ManagementService>>,
    Extension(ctx): Extension<toolkit_security::SecurityContext>,
    Body(upstream): Body<Upstream>,
) -> Result<(StatusCode, Json<Upstream>), OagwError> {
    let created = service
        .create_upstream(ctx.subject_tenant_id(), upstream)
        .map_err(OagwError::gateway)?;
    Ok((StatusCode::CREATED, Json(created)))
}

/// `GET /oagw/v1/upstreams`
///
/// Lists the tenant's upstreams.
///
/// # Errors
/// [`OagwError`] when the caller cannot be identified.
pub async fn list_upstreams(
    Extension(service): Extension<Arc<ManagementService>>,
    Extension(ctx): Extension<toolkit_security::SecurityContext>,
    axum::extract::Query(query): axum::extract::Query<ListQuery>,
) -> Result<Json<Vec<Upstream>>, OagwError> {
    let items = service.list_upstreams(ctx.subject_tenant_id(), &query.params());
    Ok(Json(items))
}

/// `GET /oagw/v1/upstreams/{id}`
///
/// Reads one upstream.
///
/// # Errors
/// [`OagwError`] `404` when unknown or foreign.
pub async fn get_upstream(
    Extension(service): Extension<Arc<ManagementService>>,
    Extension(ctx): Extension<toolkit_security::SecurityContext>,
    Path(id): Path<Uuid>,
) -> Result<Json<Upstream>, OagwError> {
    let upstream = service
        .get_upstream(ctx.subject_tenant_id(), id)
        .map_err(OagwError::gateway)?;
    Ok(Json(upstream))
}

/// `PUT /oagw/v1/upstreams/{id}`
///
/// Replaces an upstream; the alias is immutable.
///
/// # Errors
/// [`OagwError`] `400`/`404`/`409`.
pub async fn replace_upstream(
    Extension(service): Extension<Arc<ManagementService>>,
    Extension(ctx): Extension<toolkit_security::SecurityContext>,
    Path(id): Path<Uuid>,
    Body(upstream): Body<Upstream>,
) -> Result<Json<Upstream>, OagwError> {
    let replaced = service
        .replace_upstream(ctx.subject_tenant_id(), id, upstream)
        .map_err(OagwError::gateway)?;
    Ok(Json(replaced))
}

/// `DELETE /oagw/v1/upstreams/{id}`
///
/// Deletes the upstream and its routes.
///
/// # Errors
/// [`OagwError`] `404` when unknown or foreign.
pub async fn delete_upstream(
    Extension(service): Extension<Arc<ManagementService>>,
    Extension(ctx): Extension<toolkit_security::SecurityContext>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, OagwError> {
    let _deleted = service
        .delete_upstream(ctx.subject_tenant_id(), id)
        .map_err(OagwError::gateway)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /oagw/v1/routes`
///
/// Creates a route against an existing upstream.
///
/// # Errors
/// [`OagwError`] `400`/`404`/`409`.
pub async fn create_route(
    Extension(service): Extension<Arc<ManagementService>>,
    Extension(ctx): Extension<toolkit_security::SecurityContext>,
    Body(route): Body<Route>,
) -> Result<(StatusCode, Json<Route>), OagwError> {
    let created = service
        .create_route(ctx.subject_tenant_id(), route)
        .map_err(OagwError::gateway)?;
    Ok((StatusCode::CREATED, Json(created)))
}

/// `GET /oagw/v1/routes`
///
/// Lists the tenant's routes.
///
/// # Errors
/// [`OagwError`] when the caller cannot be identified.
pub async fn list_routes(
    Extension(service): Extension<Arc<ManagementService>>,
    Extension(ctx): Extension<toolkit_security::SecurityContext>,
    axum::extract::Query(query): axum::extract::Query<ListQuery>,
) -> Result<Json<Vec<Route>>, OagwError> {
    let items = service.list_routes(ctx.subject_tenant_id(), &query.params());
    Ok(Json(items))
}

/// `GET /oagw/v1/routes/{id}`
///
/// Reads one route.
///
/// # Errors
/// [`OagwError`] `404` when unknown or foreign.
pub async fn get_route(
    Extension(service): Extension<Arc<ManagementService>>,
    Extension(ctx): Extension<toolkit_security::SecurityContext>,
    Path(id): Path<Uuid>,
) -> Result<Json<Route>, OagwError> {
    let route = service
        .get_route(ctx.subject_tenant_id(), id)
        .map_err(OagwError::gateway)?;
    Ok(Json(route))
}

/// `PUT /oagw/v1/routes/{id}`
///
/// Replaces a route; the owning upstream is immutable.
///
/// # Errors
/// [`OagwError`] `400`/`404`/`409`.
pub async fn replace_route(
    Extension(service): Extension<Arc<ManagementService>>,
    Extension(ctx): Extension<toolkit_security::SecurityContext>,
    Path(id): Path<Uuid>,
    Body(route): Body<Route>,
) -> Result<Json<Route>, OagwError> {
    let replaced = service
        .replace_route(ctx.subject_tenant_id(), id, route)
        .map_err(OagwError::gateway)?;
    Ok(Json(replaced))
}

/// `DELETE /oagw/v1/routes/{id}`
///
/// Deletes a route.
///
/// # Errors
/// [`OagwError`] `404` when unknown or foreign.
pub async fn delete_route(
    Extension(service): Extension<Arc<ManagementService>>,
    Extension(ctx): Extension<toolkit_security::SecurityContext>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, OagwError> {
    let _deleted = service
        .delete_route(ctx.subject_tenant_id(), id)
        .map_err(OagwError::gateway)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /oagw/v1/plugins`
///
/// Creates an immutable custom plugin definition.
///
/// # Errors
/// [`OagwError`] `400` when malformed, `409` when the name is taken.
pub async fn create_plugin(
    Extension(service): Extension<Arc<ManagementService>>,
    Extension(ctx): Extension<toolkit_security::SecurityContext>,
    Body(plugin): Body<Plugin>,
) -> Result<(StatusCode, Json<Plugin>), OagwError> {
    let created = service
        .create_plugin(ctx.subject_tenant_id(), plugin)
        .map_err(OagwError::gateway)?;
    Ok((StatusCode::CREATED, Json(created)))
}

/// `GET /oagw/v1/plugins`
///
/// Lists the tenant's custom plugins; built-ins are never listed.
///
/// # Errors
/// [`OagwError`] when the caller cannot be identified.
pub async fn list_plugins(
    Extension(service): Extension<Arc<ManagementService>>,
    Extension(ctx): Extension<toolkit_security::SecurityContext>,
    axum::extract::Query(query): axum::extract::Query<ListQuery>,
) -> Result<Json<Vec<Plugin>>, OagwError> {
    let plugins = service.list_plugins(ctx.subject_tenant_id(), &query.params());
    Ok(Json(plugins))
}

/// `GET /oagw/v1/plugins/{id}`
///
/// Reads one custom plugin.
///
/// # Errors
/// [`OagwError`] `404` when unknown, foreign, or a built-in id.
pub async fn get_plugin(
    Extension(service): Extension<Arc<ManagementService>>,
    Extension(ctx): Extension<toolkit_security::SecurityContext>,
    Path(raw): Path<String>,
) -> Result<Json<Plugin>, OagwError> {
    let id = plugin_id(&raw)?;
    let plugin = service
        .get_plugin(ctx.subject_tenant_id(), id)
        .map_err(OagwError::gateway)?;
    Ok(Json(plugin))
}

/// `GET /oagw/v1/plugins/{id}/source`
///
/// Returns the stored Starlark source as `text/plain`.
///
/// # Errors
/// [`OagwError`] `404` when unknown, foreign, or a built-in id.
pub async fn get_plugin_source(
    Extension(service): Extension<Arc<ManagementService>>,
    Extension(ctx): Extension<toolkit_security::SecurityContext>,
    Path(raw): Path<String>,
) -> Result<Response, OagwError> {
    let id = plugin_id(&raw)?;
    let plugin = service
        .get_plugin(ctx.subject_tenant_id(), id)
        .map_err(OagwError::gateway)?;
    Ok((
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        plugin.source,
    )
        .into_response())
}

/// `DELETE /oagw/v1/plugins/{id}`
///
/// Deletes an unreferenced custom plugin.
///
/// # Errors
/// [`OagwError`] `409` when still referenced, `404` when unknown or foreign.
pub async fn delete_plugin(
    Extension(service): Extension<Arc<ManagementService>>,
    Extension(ctx): Extension<toolkit_security::SecurityContext>,
    Path(raw): Path<String>,
) -> Result<StatusCode, OagwError> {
    let id = plugin_id(&raw)?;
    let _deleted = service
        .delete_plugin(ctx.subject_tenant_id(), id)
        .map_err(OagwError::gateway)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Parses a plugin path segment as the uuid the collection is keyed by.
///
/// Built-in plugins are addressed by GTS id in a binding list but are not rows
/// in the collection, and neither is any other non-uuid form; all of them
/// answer `404` rather than a path-parsing `400`.
///
/// # Errors
/// [`DomainError::NotFound`] when the segment is not a uuid.
fn plugin_id(raw: &str) -> Result<Uuid, OagwError> {
    Uuid::parse_str(raw).map_err(|_| OagwError::gateway(DomainError::NotFound))
}

/// Rejects the update of an immutable custom plugin (`405`).
pub async fn replace_plugin_not_allowed() -> Response {
    let error = OagwError::gateway(crate::domain::error::DomainError::Validation(
        "plugins are immutable; create a new plugin instead".to_owned(),
    ));
    (StatusCode::METHOD_NOT_ALLOWED, error).into_response()
}

/// `GET /oagw/v1/healthz`
///
/// The gear's own liveness endpoint.
pub async fn healthz(
    Extension(service): Extension<Arc<crate::infra::proxy::ProxyService>>,
) -> Response {
    let metrics = service.metrics();
    Json(serde_json::json!({
        "status": "ok",
        "proxy_requests_total": metrics.requests_total(),
        "proxy_upgrades_total": metrics.upgrades_total(),
    }))
    .into_response()
}
