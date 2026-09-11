//! REST handlers for `/oagw/v1/plugins`
//! (`cpt-cf-oagw-feature-plugin-management`).
//!
//! ## Tenant scoping
//!
//! Every handler extracts the calling tenant from
//! `axum::Extension<toolkit_security::SecurityContext>`
//! (`SecurityContext::subject_tenant_id()`), the same extractor pattern
//! `src/api/rest/handlers/upstreams.rs` and `src/api/rest/handlers/routes.rs`
//! use; this feature invents no new tenant-resolution mechanism.
//!
//! ## Plugin identifiers
//!
//! Unlike upstreams and routes, a plugin's path identifier is a full GTS
//! string (`gts.cf.core.oagw.{type}_plugin.v1~{instance}`), not a bare
//! `Uuid` — the instance part may itself be a UUID (custom plugin) or a name
//! (named built-in plugin), per `cpt-cf-oagw-dod-plugin-identification`.
//! Every handler here therefore extracts `Path<String>` and hands the raw
//! GTS string to `crate::domain::service`, which resolves it via
//! `crate::domain::plugin_resolve::resolve_plugin_ref`.
//!
//! ## No replace verb
//!
//! `cpt-cf-oagw-dod-plugin-no-replace`: this module deliberately defines no
//! `replace_plugin`/`patch_plugin` handler, and
//! `src/api/rest/routes.rs` registers no `PUT`/`PATCH` route for
//! `/oagw/v1/plugins/{id}` — plugin definitions are immutable after
//! creation.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, Path, Query};
use axum::http::{StatusCode, Uri};
use serde_json::Value;
use toolkit_security::SecurityContext;

use super::tenant_id_of;
use crate::domain::model::{Plugin, PluginSource};
use crate::domain::query::RawListQuery;
use crate::domain::service;
use crate::error::OagwError;
use crate::state::ControlPlaneState;

/// `POST /oagw/v1/plugins` (`cpt-cf-oagw-dod-plugin-create`,
/// `cpt-cf-oagw-flow-register-plugin`).
///
/// # Errors
///
/// Returns [`OagwError::validation_error`] when the body fails validation,
/// and [`OagwError::plugin_name_conflict`] when `name` already exists for
/// the calling tenant (see [`service::create_plugin`]).
// @cpt-begin:cpt-cf-oagw-dod-plugin-create:p1:inst-create-plugin-handler-01
pub async fn create_plugin(
    Extension(state): Extension<Arc<ControlPlaneState>>,
    security_ctx: Option<Extension<SecurityContext>>,
    uri: Uri,
    Json(body): Json<Value>,
) -> Result<(StatusCode, Json<Plugin>), OagwError> {
    let tenant_id = tenant_id_of(security_ctx);
    let plugin = service::create_plugin(&state, tenant_id, body)
        .map_err(|error| error.with_instance(uri.path()))?;
    Ok((StatusCode::CREATED, Json(plugin)))
}
// @cpt-end:cpt-cf-oagw-dod-plugin-create:p1:inst-create-plugin-handler-01

/// `GET /oagw/v1/plugins` (`cpt-cf-oagw-dod-plugin-list`,
/// `cpt-cf-oagw-flow-list-plugins`).
///
/// # Errors
///
/// Returns [`OagwError::validation_error`] when a query parameter is
/// malformed, references an undeclared field, or `$top` exceeds 100 (see
/// [`service::list_plugins`]).
// @cpt-begin:cpt-cf-oagw-dod-plugin-list:p1:inst-list-plugins-handler-01
pub async fn list_plugins(
    Extension(state): Extension<Arc<ControlPlaneState>>,
    security_ctx: Option<Extension<SecurityContext>>,
    uri: Uri,
    Query(raw_query): Query<RawListQuery>,
) -> Result<Json<Vec<Value>>, OagwError> {
    let tenant_id = tenant_id_of(security_ctx);
    let items = service::list_plugins(&state, tenant_id, &raw_query)
        .map_err(|error| error.with_instance(uri.path()))?;
    Ok(Json(items))
}
// @cpt-end:cpt-cf-oagw-dod-plugin-list:p1:inst-list-plugins-handler-01

/// `GET /oagw/v1/plugins/{id}` (`cpt-cf-oagw-dod-plugin-get`,
/// `cpt-cf-oagw-flow-get-plugin`).
///
/// # Errors
///
/// Returns [`OagwError::plugin_record_not_found`] when `id` does not
/// resolve to a stored plugin owned by the calling tenant (see
/// [`service::get_plugin`]).
// @cpt-begin:cpt-cf-oagw-dod-plugin-get:p1:inst-get-plugin-handler-01
pub async fn get_plugin(
    Extension(state): Extension<Arc<ControlPlaneState>>,
    security_ctx: Option<Extension<SecurityContext>>,
    uri: Uri,
    Path(id): Path<String>,
) -> Result<Json<Plugin>, OagwError> {
    let tenant_id = tenant_id_of(security_ctx);
    let plugin = service::get_plugin(&state, tenant_id, &id)
        .map_err(|error| error.with_instance(uri.path()))?;
    Ok(Json(plugin))
}
// @cpt-end:cpt-cf-oagw-dod-plugin-get:p1:inst-get-plugin-handler-01

/// `GET /oagw/v1/plugins/{id}/source` (`cpt-cf-oagw-dod-plugin-get-source`,
/// `cpt-cf-oagw-flow-get-plugin-source`).
///
/// # Errors
///
/// Returns [`OagwError::plugin_record_not_found`] when `id` names a named
/// built-in identifier (which carries no stored source) or does not resolve
/// to a stored plugin owned by the calling tenant (see
/// [`service::get_plugin_source`]).
// @cpt-begin:cpt-cf-oagw-dod-plugin-get-source:p1:inst-get-plugin-source-handler-01
pub async fn get_plugin_source(
    Extension(state): Extension<Arc<ControlPlaneState>>,
    security_ctx: Option<Extension<SecurityContext>>,
    uri: Uri,
    Path(id): Path<String>,
) -> Result<Json<PluginSource>, OagwError> {
    let tenant_id = tenant_id_of(security_ctx);
    let source = service::get_plugin_source(&state, tenant_id, &id)
        .map_err(|error| error.with_instance(uri.path()))?;
    Ok(Json(source))
}
// @cpt-end:cpt-cf-oagw-dod-plugin-get-source:p1:inst-get-plugin-source-handler-01

/// `DELETE /oagw/v1/plugins/{id}` (`cpt-cf-oagw-dod-plugin-delete`,
/// `cpt-cf-oagw-flow-delete-plugin`).
///
/// # Errors
///
/// Returns [`OagwError::plugin_record_not_found`] when `id` does not
/// resolve to a stored plugin owned by the calling tenant, and
/// [`OagwError::plugin_in_use`] when the plugin is still bound to any
/// upstream or route (see [`service::delete_plugin`]).
// @cpt-begin:cpt-cf-oagw-dod-plugin-delete:p1:inst-delete-plugin-handler-01
pub async fn delete_plugin(
    Extension(state): Extension<Arc<ControlPlaneState>>,
    security_ctx: Option<Extension<SecurityContext>>,
    uri: Uri,
    Path(id): Path<String>,
) -> Result<StatusCode, OagwError> {
    let tenant_id = tenant_id_of(security_ctx);
    service::delete_plugin(&state, tenant_id, &id)
        .map_err(|error| error.with_instance(uri.path()))?;
    Ok(StatusCode::NO_CONTENT)
}
// @cpt-end:cpt-cf-oagw-dod-plugin-delete:p1:inst-delete-plugin-handler-01
