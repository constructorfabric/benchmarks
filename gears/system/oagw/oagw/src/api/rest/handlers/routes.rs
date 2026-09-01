//! Route CRUD handlers (DESIGN §5).

use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, Path, Query};
use axum::http::StatusCode;
use toolkit_security::SecurityContext;

use crate::api::rest::error::OagwProblem;
use crate::api::rest::extractors::ListQuery;
use crate::domain::dto::Route;
use crate::domain::services::management::ControlPlaneService;

use super::problem;

/// POST /oagw/v1/routes — create a route under an upstream owned by the
/// tenant.
///
/// # Errors
///
/// Returns an [`OagwProblem`] when the tenant cannot be resolved, the route
/// fails validation, or the route conflicts with an existing match pattern.
pub async fn create_route(
    Extension(control): axum::Extension<Arc<ControlPlaneService>>,
    Extension(security): axum::Extension<SecurityContext>,
    Json(input): Json<Route>,
) -> Result<(StatusCode, Json<Route>), OagwProblem> {
    let tenant_id = super::tenant_of(&security);
    let created = control
        .create_route(tenant_id, input)
        .map_err(|e| problem(&e, "/oagw/v1/routes"))?;
    Ok((StatusCode::CREATED, Json(created)))
}

/// GET /oagw/v1/routes — list routes (`OData` list query; filter by
/// `upstream_id eq '<uuid>'`).
///
/// # Errors
///
/// Returns an [`OagwProblem`] when the tenant cannot be resolved.
#[allow(clippy::implicit_hasher)] // axum Query extractor requires a concrete HashMap type here.
pub async fn list_routes(
    Extension(control): axum::Extension<Arc<ControlPlaneService>>,
    Extension(security): axum::Extension<SecurityContext>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, OagwProblem> {
    let tenant_id = super::tenant_of(&security);
    let query = ListQuery::from_params(&params);
    let upstream_id = query.filter.as_deref().and_then(|f| {
        let (field, value) = f.trim().split_once("eq")?;
        if field.trim() == "upstream_id" {
            uuid::Uuid::parse_str(value.trim().trim_matches('\'')).ok()
        } else {
            None
        }
    });
    let items = control.list_routes(tenant_id, upstream_id);
    // The generic keep predicate also applies the parsed `upstream_id`
    // filter, keeping one code path for pagination/ordering/projection.
    let out = query.apply(items, query.route_keep(), query.route_cmp());
    Ok(Json(serde_json::Value::Array(out)))
}

/// GET /oagw/v1/routes/{id} — get a single route.
///
/// # Errors
///
/// Returns an [`OagwProblem`] when the tenant cannot be resolved or the
/// route does not exist in that tenant.
pub async fn get_route(
    Extension(control): axum::Extension<Arc<ControlPlaneService>>,
    Extension(security): axum::Extension<SecurityContext>,
    Path(id): Path<uuid::Uuid>,
) -> Result<Json<Route>, OagwProblem> {
    let tenant_id = super::tenant_of(&security);
    let item = control
        .get_route(tenant_id, id)
        .map_err(|e| problem(&e, &format!("/oagw/v1/routes/{id}")))?;
    Ok(Json(item))
}

/// PUT /oagw/v1/routes/{id} — replace a route (`upstream_id` is immutable).
///
/// # Errors
///
/// Returns an [`OagwProblem`] when the tenant cannot be resolved, the route
/// does not exist, or the updated route fails validation / conflicts.
pub async fn update_route(
    Extension(control): axum::Extension<Arc<ControlPlaneService>>,
    Extension(security): axum::Extension<SecurityContext>,
    Path(id): Path<uuid::Uuid>,
    Json(input): Json<Route>,
) -> Result<Json<Route>, OagwProblem> {
    let tenant_id = super::tenant_of(&security);
    let updated = control
        .update_route(tenant_id, id, input)
        .map_err(|e| problem(&e, &format!("/oagw/v1/routes/{id}")))?;
    Ok(Json(updated))
}

/// DELETE /oagw/v1/routes/{id} — delete a route.
///
/// # Errors
///
/// Returns an [`OagwProblem`] when the tenant cannot be resolved or the
/// route does not exist in that tenant.
pub async fn delete_route(
    Extension(control): axum::Extension<Arc<ControlPlaneService>>,
    Extension(security): axum::Extension<SecurityContext>,
    Path(id): Path<uuid::Uuid>,
) -> Result<axum::response::Response, OagwProblem> {
    let tenant_id = super::tenant_of(&security);
    control
        .delete_route(tenant_id, id)
        .map_err(|e| problem(&e, &format!("/oagw/v1/routes/{id}")))?;
    Ok(super::no_content())
}
