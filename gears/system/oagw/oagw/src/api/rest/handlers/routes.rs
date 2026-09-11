//! REST handlers for `/oagw/v1/routes`
//! (`cpt-cf-oagw-feature-route-management`).
//!
//! ## Tenant scoping
//!
//! Every handler extracts the calling tenant from
//! `axum::Extension<toolkit_security::SecurityContext>`
//! (`SecurityContext::subject_tenant_id()`), the same extractor pattern
//! `src/api/rest/handlers/upstreams.rs` uses; this feature invents no new
//! tenant-resolution mechanism. The extension is bound as
//! `Option<Extension<SecurityContext>>` so a request that genuinely arrives
//! with no populated extension still resolves — to the nil UUID tenant —
//! rather than failing the whole request; authentication/authorization
//! themselves remain entirely the platform gateway's responsibility ahead of
//! this gear, per this feature's boundary note.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, Path, Query};
use axum::http::{StatusCode, Uri};
use serde_json::Value;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::tenant_id_of;
use crate::domain::model::Route;
use crate::domain::query::RawListQuery;
use crate::domain::service;
use crate::error::OagwError;
use crate::state::ControlPlaneState;

/// `POST /oagw/v1/routes` (`cpt-cf-oagw-dod-route-crud-endpoints`,
/// `cpt-cf-oagw-flow-create-route`).
///
/// # Errors
///
/// Returns [`OagwError::validation_error`] when the body fails schema
/// validation or `upstream_id` does not reference an upstream owned by the
/// calling tenant, and [`OagwError::route_conflict`] when the candidate
/// route's path, priority, and method set duplicate another enabled route
/// under the same upstream (see [`service::create_route`]).
// @cpt-begin:cpt-cf-oagw-dod-route-crud-endpoints:p1:inst-create-route-handler-01
pub async fn create_route(
    Extension(state): Extension<Arc<ControlPlaneState>>,
    security_ctx: Option<Extension<SecurityContext>>,
    uri: Uri,
    Json(body): Json<Value>,
) -> Result<(StatusCode, Json<Route>), OagwError> {
    let tenant_id = tenant_id_of(security_ctx);
    let route = service::create_route(&state, tenant_id, body)
        .map_err(|error| error.with_instance(uri.path()))?;
    Ok((StatusCode::CREATED, Json(route)))
}
// @cpt-end:cpt-cf-oagw-dod-route-crud-endpoints:p1:inst-create-route-handler-01

/// `GET /oagw/v1/routes` (`cpt-cf-oagw-dod-route-crud-endpoints`,
/// `cpt-cf-oagw-dod-route-list-query-params`, `cpt-cf-oagw-flow-list-routes`).
///
/// # Errors
///
/// Returns [`OagwError::validation_error`] when a query parameter is
/// malformed, references an undeclared field, or `$top` exceeds 100 (see
/// [`service::list_routes`]).
// @cpt-begin:cpt-cf-oagw-dod-route-crud-endpoints:p1:inst-list-routes-handler-01
pub async fn list_routes(
    Extension(state): Extension<Arc<ControlPlaneState>>,
    security_ctx: Option<Extension<SecurityContext>>,
    uri: Uri,
    Query(raw_query): Query<RawListQuery>,
) -> Result<Json<Vec<Value>>, OagwError> {
    let tenant_id = tenant_id_of(security_ctx);
    let items = service::list_routes(&state, tenant_id, &raw_query)
        .map_err(|error| error.with_instance(uri.path()))?;
    Ok(Json(items))
}
// @cpt-end:cpt-cf-oagw-dod-route-crud-endpoints:p1:inst-list-routes-handler-01

/// `GET /oagw/v1/routes/{id}` (`cpt-cf-oagw-dod-route-crud-endpoints`,
/// `cpt-cf-oagw-flow-get-route`).
///
/// # Errors
///
/// Returns [`OagwError::route_record_not_found`] when no route with `id`
/// exists for the calling tenant (see [`service::get_route`]).
// @cpt-begin:cpt-cf-oagw-dod-route-crud-endpoints:p1:inst-get-route-handler-01
pub async fn get_route(
    Extension(state): Extension<Arc<ControlPlaneState>>,
    security_ctx: Option<Extension<SecurityContext>>,
    uri: Uri,
    Path(id): Path<Uuid>,
) -> Result<Json<Route>, OagwError> {
    let tenant_id = tenant_id_of(security_ctx);
    let route = service::get_route(&state, tenant_id, id)
        .map_err(|error| error.with_instance(uri.path()))?;
    Ok(Json(route))
}
// @cpt-end:cpt-cf-oagw-dod-route-crud-endpoints:p1:inst-get-route-handler-01

/// `PUT /oagw/v1/routes/{id}` (`cpt-cf-oagw-dod-route-crud-endpoints`,
/// `cpt-cf-oagw-flow-replace-route`).
///
/// # Errors
///
/// Returns [`OagwError::route_record_not_found`] when `id` does not resolve
/// for the calling tenant, [`OagwError::validation_error`] when the body
/// fails schema validation or supplies an `upstream_id` different from the
/// stored route's, and [`OagwError::route_conflict`] when the replacement
/// would duplicate another enabled route (see [`service::replace_route`]).
// @cpt-begin:cpt-cf-oagw-dod-route-crud-endpoints:p1:inst-replace-route-handler-01
pub async fn replace_route(
    Extension(state): Extension<Arc<ControlPlaneState>>,
    security_ctx: Option<Extension<SecurityContext>>,
    uri: Uri,
    Path(id): Path<Uuid>,
    Json(body): Json<Value>,
) -> Result<Json<Route>, OagwError> {
    let tenant_id = tenant_id_of(security_ctx);
    let route = service::replace_route(&state, tenant_id, id, body)
        .map_err(|error| error.with_instance(uri.path()))?;
    Ok(Json(route))
}
// @cpt-end:cpt-cf-oagw-dod-route-crud-endpoints:p1:inst-replace-route-handler-01

/// `DELETE /oagw/v1/routes/{id}` (`cpt-cf-oagw-dod-route-crud-endpoints`,
/// `cpt-cf-oagw-flow-delete-route`).
///
/// # Errors
///
/// Returns [`OagwError::route_record_not_found`] when no route with `id`
/// exists for the calling tenant (see [`service::delete_route`]).
// @cpt-begin:cpt-cf-oagw-dod-route-crud-endpoints:p1:inst-delete-route-handler-01
pub async fn delete_route(
    Extension(state): Extension<Arc<ControlPlaneState>>,
    security_ctx: Option<Extension<SecurityContext>>,
    uri: Uri,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, OagwError> {
    let tenant_id = tenant_id_of(security_ctx);
    service::delete_route(&state, tenant_id, id)
        .map_err(|error| error.with_instance(uri.path()))?;
    Ok(StatusCode::NO_CONTENT)
}
// @cpt-end:cpt-cf-oagw-dod-route-crud-endpoints:p1:inst-delete-route-handler-01
