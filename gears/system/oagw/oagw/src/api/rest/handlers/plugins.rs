//! Custom (uuid-backed) plugin CRUD handlers (DESIGN §5).

use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::{Extension, Path, Query};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;
use toolkit_security::SecurityContext;

use crate::api::rest::error::OagwProblem;
use crate::api::rest::extractors::ListQuery;
use crate::domain::dto::Plugin;
use crate::domain::services::management::ControlPlaneService;

use super::problem;

/// POST /oagw/v1/plugins — create a custom plugin (immutable after creation).
///
/// # Errors
///
/// Returns an [`OagwProblem`] when the tenant cannot be resolved, the input
/// fails validation, or the plugin id/name conflicts with an existing entry.
pub async fn create_plugin(
    Extension(control): axum::Extension<Arc<ControlPlaneService>>,
    Extension(security): axum::Extension<SecurityContext>,
    Json(input): Json<Plugin>,
) -> Result<(StatusCode, Json<Plugin>), OagwProblem> {
    let tenant_id = super::tenant_of(&security);
    let created = control
        .create_plugin(tenant_id, input)
        .map_err(|e| problem(&e, "/oagw/v1/plugins"))?;
    Ok((StatusCode::CREATED, Json(created)))
}

/// GET /oagw/v1/plugins — list custom plugins (`OData` list query).
///
/// # Errors
///
/// Returns an [`OagwProblem`] when the tenant cannot be resolved.
#[allow(clippy::implicit_hasher)] // axum Query extractor requires a concrete HashMap type here.
pub async fn list_plugins(
    Extension(control): axum::Extension<Arc<ControlPlaneService>>,
    Extension(security): axum::Extension<SecurityContext>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, OagwProblem> {
    let tenant_id = super::tenant_of(&security);
    let query = ListQuery::from_params(&params);
    let items = control.list_plugins(tenant_id);
    let out = query.apply(items, query.plugin_keep(), query.plugin_cmp());
    Ok(Json(serde_json::Value::Array(out)))
}

/// GET /oagw/v1/plugins/{id} — get a single plugin.
///
/// # Errors
///
/// Returns an [`OagwProblem`] when the tenant cannot be resolved or the
/// plugin does not exist in that tenant.
pub async fn get_plugin(
    Extension(control): axum::Extension<Arc<ControlPlaneService>>,
    Extension(security): axum::Extension<SecurityContext>,
    Path(id): Path<uuid::Uuid>,
) -> Result<Json<Plugin>, OagwProblem> {
    let tenant_id = super::tenant_of(&security);
    let item = control
        .get_plugin(tenant_id, id)
        .map_err(|e| problem(&e, &format!("/oagw/v1/plugins/{id}")))?;
    Ok(Json(item))
}

/// GET /oagw/v1/plugins/{id}/source — get the plugin's Starlark source.
///
/// # Errors
///
/// Returns an [`OagwProblem`] when the tenant cannot be resolved or the
/// plugin does not exist in that tenant.
pub async fn get_plugin_source(
    Extension(control): axum::Extension<Arc<ControlPlaneService>>,
    Extension(security): axum::Extension<SecurityContext>,
    Path(id): Path<uuid::Uuid>,
) -> Result<Response, OagwProblem> {
    let tenant_id = super::tenant_of(&security);
    let item = control
        .get_plugin(tenant_id, id)
        .map_err(|e| problem(&e, &format!("/oagw/v1/plugins/{id}/source")))?;
    let mut response = Response::new(Body::from(item.source_code));
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    Ok(response)
}

/// DELETE /oagw/v1/plugins/{id} — delete a plugin (409 when referenced).
///
/// # Errors
///
/// Returns an [`OagwProblem`] when the tenant cannot be resolved, the plugin
/// does not exist, or the plugin is still referenced (`PluginInUse`).
pub async fn delete_plugin(
    Extension(control): axum::Extension<Arc<ControlPlaneService>>,
    Extension(security): axum::Extension<SecurityContext>,
    Path(id): Path<uuid::Uuid>,
) -> Result<axum::response::Response, OagwProblem> {
    let tenant_id = super::tenant_of(&security);
    control
        .delete_plugin(tenant_id, id)
        .map_err(|e| problem(&e, &format!("/oagw/v1/plugins/{id}")))?;
    Ok(super::no_content())
}
