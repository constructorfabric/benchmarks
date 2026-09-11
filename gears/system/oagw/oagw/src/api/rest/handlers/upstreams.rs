//! REST handlers for `/oagw/v1/upstreams`
//! (`cpt-cf-oagw-feature-upstream-management`).
//!
//! ## Tenant scoping
//!
//! Every handler extracts the calling tenant from
//! `axum::Extension<toolkit_security::SecurityContext>`
//! (`SecurityContext::subject_tenant_id()`), the same extractor pattern
//! `resource-group`'s and `credstore`'s REST handlers use — this feature
//! invents no new tenant-resolution mechanism. The extension is bound as
//! `Option<Extension<SecurityContext>>` so a request that genuinely arrives
//! with no populated extension (e.g. this gear mounted standalone, ahead of
//! the platform's auth middleware) still resolves — to the nil UUID tenant —
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
use crate::domain::model::Upstream;
use crate::domain::query::RawListQuery;
use crate::domain::service;
use crate::error::OagwError;
use crate::state::ControlPlaneState;

/// `POST /oagw/v1/upstreams` (`cpt-cf-oagw-dod-create-upstream-endpoint`,
/// `cpt-cf-oagw-flow-create-upstream`).
///
/// # Errors
///
/// Returns [`OagwError::validation_error`] when the body fails schema
/// validation or alias derivation, and [`OagwError::alias_conflict`] when
/// the resolved alias already exists for the calling tenant (see
/// [`service::create_upstream`]).
// @cpt-begin:cpt-cf-oagw-dod-create-upstream-endpoint:p1:inst-create-upstream-handler-01
pub async fn create_upstream(
    Extension(state): Extension<Arc<ControlPlaneState>>,
    security_ctx: Option<Extension<SecurityContext>>,
    uri: Uri,
    Json(body): Json<Value>,
) -> Result<(StatusCode, Json<Upstream>), OagwError> {
    let tenant_id = tenant_id_of(security_ctx);
    let upstream = service::create_upstream(&state, tenant_id, body)
        .map_err(|error| error.with_instance(uri.path()))?;
    Ok((StatusCode::CREATED, Json(upstream)))
}
// @cpt-end:cpt-cf-oagw-dod-create-upstream-endpoint:p1:inst-create-upstream-handler-01

/// `GET /oagw/v1/upstreams` (`cpt-cf-oagw-dod-list-upstreams-endpoint`,
/// `cpt-cf-oagw-flow-list-upstreams`).
///
/// # Errors
///
/// Returns [`OagwError::validation_error`] when a query parameter is
/// malformed, references an undeclared field, or `$top` exceeds 100 (see
/// [`service::list_upstreams`]).
// @cpt-begin:cpt-cf-oagw-dod-list-upstreams-endpoint:p1:inst-list-upstreams-handler-01
pub async fn list_upstreams(
    Extension(state): Extension<Arc<ControlPlaneState>>,
    security_ctx: Option<Extension<SecurityContext>>,
    uri: Uri,
    Query(raw_query): Query<RawListQuery>,
) -> Result<Json<Vec<Value>>, OagwError> {
    let tenant_id = tenant_id_of(security_ctx);
    let items = service::list_upstreams(&state, tenant_id, &raw_query)
        .map_err(|error| error.with_instance(uri.path()))?;
    Ok(Json(items))
}
// @cpt-end:cpt-cf-oagw-dod-list-upstreams-endpoint:p1:inst-list-upstreams-handler-01

/// `GET /oagw/v1/upstreams/{id}` (`cpt-cf-oagw-dod-get-upstream-endpoint`,
/// `cpt-cf-oagw-flow-get-upstream`).
///
/// # Errors
///
/// Returns [`OagwError::upstream_not_found`] when no upstream with `id`
/// exists for the calling tenant (see [`service::get_upstream`]).
// @cpt-begin:cpt-cf-oagw-dod-get-upstream-endpoint:p1:inst-get-upstream-handler-01
pub async fn get_upstream(
    Extension(state): Extension<Arc<ControlPlaneState>>,
    security_ctx: Option<Extension<SecurityContext>>,
    uri: Uri,
    Path(id): Path<Uuid>,
) -> Result<Json<Upstream>, OagwError> {
    let tenant_id = tenant_id_of(security_ctx);
    let upstream = service::get_upstream(&state, tenant_id, id)
        .map_err(|error| error.with_instance(uri.path()))?;
    Ok(Json(upstream))
}
// @cpt-end:cpt-cf-oagw-dod-get-upstream-endpoint:p1:inst-get-upstream-handler-01

/// `PUT /oagw/v1/upstreams/{id}` (`cpt-cf-oagw-dod-replace-upstream-endpoint`,
/// `cpt-cf-oagw-flow-replace-upstream`).
///
/// # Errors
///
/// Returns [`OagwError::upstream_not_found`] when `id` does not resolve for
/// the calling tenant, and [`OagwError::validation_error`] when the body
/// fails schema validation, the endpoint change would alter the derived
/// alias, or the request would re-enable an ancestor-disabled alias (see
/// [`service::replace_upstream`]).
// @cpt-begin:cpt-cf-oagw-dod-replace-upstream-endpoint:p1:inst-replace-upstream-handler-01
pub async fn replace_upstream(
    Extension(state): Extension<Arc<ControlPlaneState>>,
    security_ctx: Option<Extension<SecurityContext>>,
    uri: Uri,
    Path(id): Path<Uuid>,
    Json(body): Json<Value>,
) -> Result<Json<Upstream>, OagwError> {
    let tenant_id = tenant_id_of(security_ctx);
    let upstream = service::replace_upstream(&state, tenant_id, id, body)
        .map_err(|error| error.with_instance(uri.path()))?;
    Ok(Json(upstream))
}
// @cpt-end:cpt-cf-oagw-dod-replace-upstream-endpoint:p1:inst-replace-upstream-handler-01

/// `DELETE /oagw/v1/upstreams/{id}` (`cpt-cf-oagw-dod-delete-upstream-endpoint`,
/// `cpt-cf-oagw-flow-delete-upstream`).
///
/// # Errors
///
/// Returns [`OagwError::upstream_not_found`] when no upstream with `id`
/// exists for the calling tenant (see [`service::delete_upstream`]).
// @cpt-begin:cpt-cf-oagw-dod-delete-upstream-endpoint:p1:inst-delete-upstream-handler-01
pub async fn delete_upstream(
    Extension(state): Extension<Arc<ControlPlaneState>>,
    security_ctx: Option<Extension<SecurityContext>>,
    uri: Uri,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, OagwError> {
    let tenant_id = tenant_id_of(security_ctx);
    service::delete_upstream(&state, tenant_id, id)
        .map_err(|error| error.with_instance(uri.path()))?;
    Ok(StatusCode::NO_CONTENT)
}
// @cpt-end:cpt-cf-oagw-dod-delete-upstream-endpoint:p1:inst-delete-upstream-handler-01
