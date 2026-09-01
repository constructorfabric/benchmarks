//! Upstream CRUD handlers (DESIGN §5).

use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, Path, Query};
use axum::http::StatusCode;
use toolkit_security::SecurityContext;

use crate::api::rest::error::OagwProblem;
use crate::api::rest::extractors::ListQuery;
use crate::domain::dto::Upstream;
use crate::domain::services::management::ControlPlaneService;

use super::problem;

/// POST /oagw/v1/upstreams — create an upstream (alias derivation /
/// enforcement).
///
/// # Errors
///
/// Returns an [`OagwProblem`] when the tenant cannot be resolved, the input
/// fails validation, or the upstream alias conflicts with an existing entry.
pub async fn create_upstream(
    Extension(control): axum::Extension<Arc<ControlPlaneService>>,
    Extension(security): axum::Extension<SecurityContext>,
    Json(input): Json<Upstream>,
) -> Result<(StatusCode, Json<Upstream>), OagwProblem> {
    let tenant_id = super::tenant_of(&security);
    let created = control
        .create_upstream(&security, tenant_id, input)
        .await
        .map_err(|e| problem(&e, "/oagw/v1/upstreams"))?;
    Ok((StatusCode::CREATED, Json(created)))
}

/// GET /oagw/v1/upstreams — list upstreams (`OData` list query).
///
/// # Errors
///
/// Returns an [`OagwProblem`] when the tenant cannot be resolved.
#[allow(clippy::implicit_hasher)] // axum Query extractor requires a concrete HashMap type here.
pub async fn list_upstreams(
    Extension(control): axum::Extension<Arc<ControlPlaneService>>,
    Extension(security): axum::Extension<SecurityContext>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, OagwProblem> {
    let tenant_id = super::tenant_of(&security);
    let query = ListQuery::from_params(&params);
    let items = control.list_upstreams(tenant_id);
    let out = query.apply(items, query.upstream_keep(), query.upstream_cmp());
    Ok(Json(serde_json::Value::Array(out)))
}

/// GET /oagw/v1/upstreams/{id} — get a single upstream.
///
/// # Errors
///
/// Returns an [`OagwProblem`] when the tenant cannot be resolved or the
/// upstream does not exist in that tenant.
pub async fn get_upstream(
    Extension(control): axum::Extension<Arc<ControlPlaneService>>,
    Extension(security): axum::Extension<SecurityContext>,
    Path(id): Path<uuid::Uuid>,
) -> Result<Json<Upstream>, OagwProblem> {
    let tenant_id = super::tenant_of(&security);
    let item = control
        .get_upstream(tenant_id, id)
        .map_err(|e| problem(&e, &format!("/oagw/v1/upstreams/{id}")))?;
    Ok(Json(item))
}

/// PUT /oagw/v1/upstreams/{id} — replace an upstream.
///
/// # Errors
///
/// Returns an [`OagwProblem`] when the tenant cannot be resolved, the
/// upstream does not exist, or the updated upstream fails validation /
/// conflicts with an existing alias.
pub async fn update_upstream(
    Extension(control): axum::Extension<Arc<ControlPlaneService>>,
    Extension(security): axum::Extension<SecurityContext>,
    Path(id): Path<uuid::Uuid>,
    Json(input): Json<Upstream>,
) -> Result<Json<Upstream>, OagwProblem> {
    let tenant_id = super::tenant_of(&security);
    let updated = control
        .update_upstream(&security, tenant_id, id, input)
        .await
        .map_err(|e| problem(&e, &format!("/oagw/v1/upstreams/{id}")))?;
    Ok(Json(updated))
}

/// DELETE /oagw/v1/upstreams/{id} — delete an upstream (cascades to routes).
///
/// # Errors
///
/// Returns an [`OagwProblem`] when the tenant cannot be resolved or the
/// upstream does not exist in that tenant.
pub async fn delete_upstream(
    Extension(control): axum::Extension<Arc<ControlPlaneService>>,
    Extension(security): axum::Extension<SecurityContext>,
    Path(id): Path<uuid::Uuid>,
) -> Result<axum::response::Response, OagwProblem> {
    let tenant_id = super::tenant_of(&security);
    control
        .delete_upstream(tenant_id, id)
        .map_err(|e| problem(&e, &format!("/oagw/v1/upstreams/{id}")))?;
    Ok(super::no_content())
}
