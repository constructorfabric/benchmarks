//! Management API handlers.
//!
//! Every handler is tenant-scoped: the caller's `SecurityContext` names the
//! tenant, and the store only ever exposes resources owned by it.

use std::sync::Arc;

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Path, Query};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use toolkit_security::SecurityContext;

use super::query::ListQuery;
use crate::domain::error::OagwError;
use crate::domain::model::{Plugin, Route, Upstream};
use crate::proxy::ProxyState;

type SharedState = Arc<ProxyState>;

fn tenant_of(security: &SecurityContext) -> uuid::Uuid {
    security.subject_tenant_id()
}

/// Unwraps a JSON body, mapping every extractor rejection onto the OAGW
/// validation problem.
///
/// Axum answers a body that parses but fails to deserialize with 422, and a
/// missing content type with 415. The specification puts every request
/// validation failure on 400, so the rejections are re-labelled here rather
/// than surfaced as-is.
fn parsed<T>(body: Result<Json<T>, JsonRejection>) -> Result<T, OagwError> {
    body.map(|Json(value)| value)
        .map_err(|rejection| OagwError::validation(rejection.body_text()))
}

// ---------------------------------------------------------------------------
// Upstreams
// ---------------------------------------------------------------------------

/// `POST /upstreams`
///
/// # Errors
///
/// 400 for invalid configuration, 409 on an alias conflict.
pub async fn create_upstream(
    Extension(state): Extension<SharedState>,
    Extension(security): Extension<SecurityContext>,
    body: Result<Json<Upstream>, JsonRejection>,
) -> Result<impl IntoResponse, OagwError> {
    let upstream = parsed(body)?;
    let created = state
        .store
        .create_upstream(tenant_of(&security), upstream, &state.config)?;
    Ok((StatusCode::CREATED, Json(created)))
}

/// `GET /upstreams`
///
/// # Errors
///
/// 400 for a malformed list query.
pub async fn list_upstreams(
    Extension(state): Extension<SharedState>,
    Extension(security): Extension<SecurityContext>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<super::dto::ListResponse>, OagwError> {
    let query = ListQuery::parse(&params)?;
    let items = state
        .store
        .list_upstreams(tenant_of(&security))
        .iter()
        .map(serialize)
        .collect();
    Ok(Json(super::dto::list(items, &query)))
}

/// `GET /upstreams/{id}`
///
/// # Errors
///
/// 404 when the upstream does not exist in the calling tenant.
pub async fn get_upstream(
    Extension(state): Extension<SharedState>,
    Extension(security): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, OagwError> {
    let upstream = state.store.get_upstream(tenant_of(&security), &id)?;
    Ok(Json(serialize(&upstream)))
}

/// `PUT /upstreams/{id}`
///
/// # Errors
///
/// 404 when missing, 400 for invalid configuration or an illegal alias
/// transition.
pub async fn replace_upstream(
    Extension(state): Extension<SharedState>,
    Extension(security): Extension<SecurityContext>,
    Path(id): Path<String>,
    body: Result<Json<Upstream>, JsonRejection>,
) -> Result<Json<serde_json::Value>, OagwError> {
    let upstream = parsed(body)?;
    let replaced =
        state
            .store
            .update_upstream(tenant_of(&security), &id, upstream, &state.config)?;
    Ok(Json(serialize(&replaced)))
}

/// `DELETE /upstreams/{id}`
///
/// # Errors
///
/// 404 when missing.
pub async fn delete_upstream(
    Extension(state): Extension<SharedState>,
    Extension(security): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Result<StatusCode, OagwError> {
    state.store.delete_upstream(tenant_of(&security), &id)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// `POST /routes`
///
/// # Errors
///
/// 400 for invalid configuration, 409 on a duplicate match rule.
pub async fn create_route(
    Extension(state): Extension<SharedState>,
    Extension(security): Extension<SecurityContext>,
    body: Result<Json<Route>, JsonRejection>,
) -> Result<impl IntoResponse, OagwError> {
    let route = parsed(body)?;
    let created = state.store.create_route(tenant_of(&security), route)?;
    Ok((StatusCode::CREATED, Json(created)))
}

/// `GET /routes`
///
/// # Errors
///
/// 400 for a malformed list query.
pub async fn list_routes(
    Extension(state): Extension<SharedState>,
    Extension(security): Extension<SecurityContext>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<super::dto::ListResponse>, OagwError> {
    let query = ListQuery::parse(&params)?;
    let items = state
        .store
        .list_routes(tenant_of(&security))
        .iter()
        .map(serialize)
        .collect();
    Ok(Json(super::dto::list(items, &query)))
}

/// `GET /routes/{id}`
///
/// # Errors
///
/// 404 when missing.
pub async fn get_route(
    Extension(state): Extension<SharedState>,
    Extension(security): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, OagwError> {
    let route = state.store.get_route(tenant_of(&security), &id)?;
    Ok(Json(serialize(&route)))
}

/// `PUT /routes/{id}`
///
/// # Errors
///
/// 404 when missing, 400 for invalid configuration, 409 on conflicts.
pub async fn replace_route(
    Extension(state): Extension<SharedState>,
    Extension(security): Extension<SecurityContext>,
    Path(id): Path<String>,
    body: Result<Json<Route>, JsonRejection>,
) -> Result<Json<serde_json::Value>, OagwError> {
    let route = parsed(body)?;
    let replaced = state.store.update_route(tenant_of(&security), &id, route)?;
    Ok(Json(serialize(&replaced)))
}

/// `DELETE /routes/{id}`
///
/// # Errors
///
/// 404 when missing.
pub async fn delete_route(
    Extension(state): Extension<SharedState>,
    Extension(security): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Result<StatusCode, OagwError> {
    state.store.delete_route(tenant_of(&security), &id)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

/// `POST /plugins`
///
/// # Errors
///
/// 400 for an invalid definition, 409 on a duplicate name.
pub async fn create_plugin(
    Extension(state): Extension<SharedState>,
    Extension(security): Extension<SecurityContext>,
    body: Result<Json<Plugin>, JsonRejection>,
) -> Result<impl IntoResponse, OagwError> {
    let plugin = parsed(body)?;
    let created = state.store.create_plugin(tenant_of(&security), plugin)?;
    Ok((StatusCode::CREATED, Json(created)))
}

/// `GET /plugins`
///
/// # Errors
///
/// 400 for a malformed list query.
pub async fn list_plugins(
    Extension(state): Extension<SharedState>,
    Extension(security): Extension<SecurityContext>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<super::dto::ListResponse>, OagwError> {
    let query = ListQuery::parse(&params)?;
    let items = state
        .store
        .list_plugins(tenant_of(&security))
        .iter()
        .map(serialize)
        .collect();
    Ok(Json(super::dto::list(items, &query)))
}

/// `GET /plugins/{id}`
///
/// # Errors
///
/// 404 when missing.
pub async fn get_plugin(
    Extension(state): Extension<SharedState>,
    Extension(security): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, OagwError> {
    let plugin = state.store.get_plugin(tenant_of(&security), &id)?;
    Ok(Json(serialize(&plugin)))
}

/// `GET /plugins/{id}/source`
///
/// # Errors
///
/// 404 when the plugin is missing or carries no Starlark source.
pub async fn get_plugin_source(
    Extension(state): Extension<SharedState>,
    Extension(security): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, OagwError> {
    let plugin = state.store.get_plugin(tenant_of(&security), &id)?;
    let Some(source) = plugin.source_code.clone() else {
        return Err(OagwError::plugin_not_found_api(format!(
            "plugin {id} has no Starlark source"
        )));
    };
    Ok(Json(serde_json::json!({
        "plugin_id": plugin.id.clone().unwrap_or_default(),
        "name": plugin.name,
        "plugin_type": plugin.plugin_type,
        "source": source,
        "phases": plugin.phases,
        "config_schema": plugin.config_schema,
    })))
}

/// `DELETE /plugins/{id}`
///
/// # Errors
///
/// 404 when missing, 409 while the plugin is still bound.
pub async fn delete_plugin(
    Extension(state): Extension<SharedState>,
    Extension(security): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Result<StatusCode, OagwError> {
    state.store.delete_plugin(tenant_of(&security), &id)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Serializes a resource to its JSON representation.
fn serialize<T: serde::Serialize>(value: &T) -> serde_json::Value {
    serde_json::to_value(value).unwrap_or(serde_json::Value::Null)
}
