//! Handlers for the OAGW management and proxy endpoints (DESIGN §3.3).
//!
//! All management handlers operate strictly in the calling tenant's scope
//! (from the injected [`SecurityContext`]); ancestor resources are invisible
//! (404) per the DESIGN tenant-scoping table. Handler responses use
//! [`OagwProblem`] so every gateway error carries a GTS `type` identifier and
//! the `X-OAGW-Error-Source: gateway` header.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, Request};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use http::Uri;
use serde_json::Value;
use toolkit::api::response::{created_json, no_content};
use toolkit_security::SecurityContext;

use crate::api::rest::error::{OagwProblem, domain_error_to_problem, proxy_error_to_problem};
use crate::api::rest::odata::{FieldAccessor, ListParams, apply_filter_order, project};
use crate::domain::control::ControlPlaneService;
use crate::domain::model::{Plugin, Route, Upstream};
use crate::gts;
use crate::infra::data_plane::DataPlaneService;

/// Helper: a `400 Validation` problem from a list-parameter or field error.
fn bad_request(detail: impl Into<String>) -> OagwProblem {
    OagwProblem::new(
        gts::ERR_VALIDATION,
        StatusCode::BAD_REQUEST,
        "Bad Request",
        detail,
    )
}

/// Helper: a `500 Internal` problem (never leaks the diagnostic).
fn internal_error(detail: impl Into<String>) -> OagwProblem {
    tracing::error!("oagw management handler internal error: {}", detail.into());
    OagwProblem::new(
        gts::ERR_INTERNAL,
        StatusCode::INTERNAL_SERVER_ERROR,
        "Internal Server Error",
        "internal gateway error",
    )
}

/// Helper: map a JSON-extractor rejection onto a gateway problem envelope.
///
/// A missing `Content-Type: application/json` is a `415`; malformed or
/// unrepresentable payloads are `400` (both still `application/problem+json`
/// with `x-oagw-error-source: gateway`).
fn json_rejection_problem(rejection: JsonRejection) -> OagwProblem {
    match rejection {
        JsonRejection::MissingJsonContentType(_) => OagwProblem::new(
            gts::ERR_VALIDATION,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Unsupported Media Type",
            "this endpoint requires a JSON request body (Content-Type: application/json)",
        ),
        other => OagwProblem::new(
            gts::ERR_VALIDATION,
            StatusCode::BAD_REQUEST,
            "Bad Request",
            other.body_text(),
        ),
    }
}

/// Serialize an entity to its wire JSON.
///
/// # Errors
///
/// Returns a `500 Internal Server Error` problem when serialization fails
/// (diagnostics are logged, never leaked).
#[allow(clippy::result_large_err)]
fn to_value<T: serde::Serialize>(entity: &T) -> Result<Value, OagwProblem> {
    serde_json::to_value(entity).map_err(|e| internal_error(format!("serialize: {e}")))
}

fn upstream_id(u: &Upstream) -> String {
    u.id.clone().unwrap_or_default()
}
fn upstream_alias(u: &Upstream) -> String {
    u.alias.clone().unwrap_or_default()
}
fn upstream_enabled(u: &Upstream) -> String {
    u.enabled.to_string()
}
fn upstream_protocol(u: &Upstream) -> String {
    u.protocol.clone()
}
fn route_id(r: &Route) -> String {
    r.id.clone().unwrap_or_default()
}
fn route_upstream_id(r: &Route) -> String {
    r.upstream_id.clone()
}
fn route_enabled(r: &Route) -> String {
    r.enabled.to_string()
}
fn route_priority(r: &Route) -> String {
    r.priority.to_string()
}
fn plugin_id(p: &Plugin) -> String {
    p.id.clone().unwrap_or_default()
}
fn plugin_name(p: &Plugin) -> String {
    p.name.clone()
}
fn plugin_type(p: &Plugin) -> String {
    match p.kind {
        crate::domain::model::PluginKind::Auth => "auth".to_owned(),
        crate::domain::model::PluginKind::Guard => "guard".to_owned(),
        crate::domain::model::PluginKind::Transform => "transform".to_owned(),
    }
}

/// Fields available for `$filter`/`$orderby` on upstreams.
pub static UPSTREAM_FIELDS: &[FieldAccessor<Upstream>] = &[
    FieldAccessor {
        name: "id",
        get: upstream_id,
        case_insensitive: false,
    },
    FieldAccessor {
        name: "alias",
        get: upstream_alias,
        case_insensitive: true,
    },
    FieldAccessor {
        name: "enabled",
        get: upstream_enabled,
        case_insensitive: false,
    },
    FieldAccessor {
        name: "protocol",
        get: upstream_protocol,
        case_insensitive: false,
    },
];

/// Fields available for `$filter`/`$orderby` on routes.
pub static ROUTE_FIELDS: &[FieldAccessor<Route>] = &[
    FieldAccessor {
        name: "id",
        get: route_id,
        case_insensitive: false,
    },
    FieldAccessor {
        name: "upstream_id",
        get: route_upstream_id,
        case_insensitive: false,
    },
    FieldAccessor {
        name: "enabled",
        get: route_enabled,
        case_insensitive: false,
    },
    FieldAccessor {
        name: "priority",
        get: route_priority,
        case_insensitive: false,
    },
];

/// Fields available for `$filter`/`$orderby` on plugins. `type` is accepted
/// as an alias of `plugin_type` (DESIGN list example uses `type eq 'guard'`).
pub static PLUGIN_FIELDS: &[FieldAccessor<Plugin>] = &[
    FieldAccessor {
        name: "id",
        get: plugin_id,
        case_insensitive: false,
    },
    FieldAccessor {
        name: "name",
        get: plugin_name,
        case_insensitive: true,
    },
    FieldAccessor {
        name: "plugin_type",
        get: plugin_type,
        case_insensitive: false,
    },
    FieldAccessor {
        name: "type",
        get: plugin_type,
        case_insensitive: false,
    },
];

/// Parse and apply the list-query parameters shared by all list endpoints.
///
/// # Errors
///
/// Returns a `400 Bad Request` problem for malformed `$filter` / `$orderby` /
/// `$top` / `$skip` values or unknown field names.
#[allow(clippy::result_large_err)]
fn apply_list<T: Clone>(
    entities: Vec<T>,
    raw: &HashMap<String, String>,
    fields: &[FieldAccessor<T>],
) -> Result<Vec<T>, OagwProblem> {
    let params = ListParams::parse(raw).map_err(bad_request)?;
    apply_filter_order(entities, &params, fields).map_err(bad_request)
}

/// Project the selected fields onto a serialized value.
fn with_selection(value: &Value, raw: &HashMap<String, String>) -> Value {
    let select = raw
        .get("$select")
        .map(String::as_str)
        .filter(|s| !s.is_empty());
    project(value, select)
}

// ===========================================================================
// Upstreams
// ===========================================================================

/// List the caller's upstreams.
///
/// # Errors
///
/// Returns a `400` problem for malformed list-query parameters and any
/// 5xx-domain problem from the control plane.
#[allow(clippy::implicit_hasher)]
pub async fn list_upstreams(
    Extension(ctx): Extension<SecurityContext>,
    Extension(control): Extension<Arc<ControlPlaneService>>,
    Query(raw): Query<HashMap<String, String>>,
) -> Result<Json<Value>, OagwProblem> {
    let tenant = ctx.subject_tenant_id();
    let records = control
        .list_upstreams(tenant)
        .await
        .map_err(domain_error_to_problem)?;
    let entities: Vec<Upstream> = records.into_iter().map(|r| r.entity).collect();
    let filtered = apply_list(entities, &raw, UPSTREAM_FIELDS)?;
    let values: Vec<Value> = filtered.iter().map(to_value).collect::<Result<_, _>>()?;
    let projected: Vec<Value> = values.iter().map(|v| with_selection(v, &raw)).collect();
    Ok(Json(Value::Array(projected)))
}

/// Fetch one caller-owned upstream.
///
/// # Errors
///
/// Returns a `400` problem for a malformed id and a `404` when the upstream
/// is not in the calling tenant's scope.
#[allow(clippy::implicit_hasher)]
pub async fn get_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(control): Extension<Arc<ControlPlaneService>>,
    Query(raw): Query<HashMap<String, String>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, OagwProblem> {
    let tenant = ctx.subject_tenant_id();
    let uuid = gts::parse_resource_id(&id)
        .ok_or_else(|| bad_request(format!("invalid upstream id {id:?}")))?;
    let record = control
        .get_upstream(tenant, uuid)
        .await
        .map_err(domain_error_to_problem)?;
    Ok(Json(with_selection(&to_value(&record.entity)?, &raw)))
}

/// Create an upstream.
///
/// # Errors
///
/// Returns a `400` problem for validation failures and a `409` when the
/// derived or user-supplied alias conflicts with an existing upstream.
pub async fn create_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(control): Extension<Arc<ControlPlaneService>>,
    uri: Uri,
    body: Result<Json<Upstream>, JsonRejection>,
) -> Result<impl IntoResponse, OagwProblem> {
    let tenant = ctx.subject_tenant_id();
    let Json(input) = body.map_err(json_rejection_problem)?;
    let record = control
        .create_upstream(&ctx, tenant, input)
        .await
        .map_err(domain_error_to_problem)?;
    let new_id = record.entity.id.as_deref().unwrap_or_default();
    Ok(created_json(to_value(&record.entity)?, &uri, new_id))
}

/// Replace an upstream (alias immutable).
///
/// # Errors
///
/// Returns a `400` problem for validation failures, a `404` when the
/// upstream is not in scope, and a `409` on immutable-field or alias
/// conflicts.
pub async fn update_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(control): Extension<Arc<ControlPlaneService>>,
    Path(id): Path<String>,
    body: Result<Json<Upstream>, JsonRejection>,
) -> Result<Json<Value>, OagwProblem> {
    let tenant = ctx.subject_tenant_id();
    let uuid = gts::parse_resource_id(&id)
        .ok_or_else(|| bad_request(format!("invalid upstream id {id:?}")))?;
    let Json(input) = body.map_err(json_rejection_problem)?;
    let record = control
        .update_upstream(&ctx, tenant, uuid, input)
        .await
        .map_err(domain_error_to_problem)?;
    Ok(Json(to_value(&record.entity)?))
}

/// Delete an upstream (and its routes).
///
/// # Errors
///
/// Returns a `404` problem when the upstream is not in the calling tenant's
/// scope.
pub async fn delete_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(control): Extension<Arc<ControlPlaneService>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, OagwProblem> {
    let tenant = ctx.subject_tenant_id();
    let uuid = gts::parse_resource_id(&id)
        .ok_or_else(|| bad_request(format!("invalid upstream id {id:?}")))?;
    control
        .delete_upstream(tenant, uuid)
        .await
        .map_err(domain_error_to_problem)?;
    Ok(no_content())
}

// ===========================================================================
// Routes
// ===========================================================================

/// List the caller's routes.
///
/// # Errors
///
/// Returns a `400` problem for malformed list-query parameters and any
/// 5xx-domain problem from the control plane.
#[allow(clippy::implicit_hasher)]
pub async fn list_routes(
    Extension(ctx): Extension<SecurityContext>,
    Extension(control): Extension<Arc<ControlPlaneService>>,
    Query(raw): Query<HashMap<String, String>>,
) -> Result<Json<Value>, OagwProblem> {
    let tenant = ctx.subject_tenant_id();
    let records = control
        .list_routes(tenant)
        .await
        .map_err(domain_error_to_problem)?;
    let entities: Vec<Route> = records.into_iter().map(|r| r.entity).collect();
    let filtered = apply_list(entities, &raw, ROUTE_FIELDS)?;
    let values: Vec<Value> = filtered.iter().map(to_value).collect::<Result<_, _>>()?;
    let projected: Vec<Value> = values.iter().map(|v| with_selection(v, &raw)).collect();
    Ok(Json(Value::Array(projected)))
}

/// Fetch one caller-owned route.
///
/// # Errors
///
/// Returns a `400` problem for a malformed id and a `404` when the route is
/// not in the calling tenant's scope.
pub async fn get_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(control): Extension<Arc<ControlPlaneService>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, OagwProblem> {
    let tenant = ctx.subject_tenant_id();
    let uuid = gts::parse_resource_id(&id)
        .ok_or_else(|| bad_request(format!("invalid route id {id:?}")))?;
    let record = control
        .get_route(tenant, uuid)
        .await
        .map_err(domain_error_to_problem)?;
    Ok(Json(to_value(&record.entity)?))
}

/// Create a route on a caller-owned upstream.
///
/// # Errors
///
/// Returns a `400` problem for validation failures, a `404` when the
/// referenced upstream is not in scope, and a `409` on route conflicts.
pub async fn create_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(control): Extension<Arc<ControlPlaneService>>,
    uri: Uri,
    body: Result<Json<Route>, JsonRejection>,
) -> Result<impl IntoResponse, OagwProblem> {
    let tenant = ctx.subject_tenant_id();
    let Json(input) = body.map_err(json_rejection_problem)?;
    let record = control
        .create_route(tenant, input)
        .await
        .map_err(domain_error_to_problem)?;
    let new_id = record.entity.id.as_deref().unwrap_or_default();
    Ok(created_json(to_value(&record.entity)?, &uri, new_id))
}

/// Replace a route (upstream binding immutable).
///
/// # Errors
///
/// Returns a `400` problem for validation failures, a `404` when the route
/// is not in scope, and a `409` on immutable-field conflicts.
pub async fn update_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(control): Extension<Arc<ControlPlaneService>>,
    Path(id): Path<String>,
    body: Result<Json<Route>, JsonRejection>,
) -> Result<Json<Value>, OagwProblem> {
    let tenant = ctx.subject_tenant_id();
    let uuid = gts::parse_resource_id(&id)
        .ok_or_else(|| bad_request(format!("invalid route id {id:?}")))?;
    let Json(input) = body.map_err(json_rejection_problem)?;
    let record = control
        .update_route(tenant, uuid, input)
        .await
        .map_err(domain_error_to_problem)?;
    Ok(Json(to_value(&record.entity)?))
}

/// Delete a route.
///
/// # Errors
///
/// Returns a `404` problem when the route is not in the calling tenant's
/// scope.
pub async fn delete_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(control): Extension<Arc<ControlPlaneService>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, OagwProblem> {
    let tenant = ctx.subject_tenant_id();
    let uuid = gts::parse_resource_id(&id)
        .ok_or_else(|| bad_request(format!("invalid route id {id:?}")))?;
    control
        .delete_route(tenant, uuid)
        .await
        .map_err(domain_error_to_problem)?;
    Ok(no_content())
}

// ===========================================================================
// Plugins
// ===========================================================================

/// List the caller's custom plugins.
///
/// # Errors
///
/// Returns a `400` problem for malformed list-query parameters and any
/// 5xx-domain problem from the control plane.
#[allow(clippy::implicit_hasher)]
pub async fn list_plugins(
    Extension(ctx): Extension<SecurityContext>,
    Extension(control): Extension<Arc<ControlPlaneService>>,
    Query(raw): Query<HashMap<String, String>>,
) -> Result<Json<Value>, OagwProblem> {
    let tenant = ctx.subject_tenant_id();
    let records = control
        .list_plugins(tenant)
        .await
        .map_err(domain_error_to_problem)?;
    let entities: Vec<Plugin> = records.into_iter().map(|r| r.entity).collect();
    let filtered = apply_list(entities, &raw, PLUGIN_FIELDS)?;
    let values: Vec<Value> = filtered.iter().map(to_value).collect::<Result<_, _>>()?;
    let projected: Vec<Value> = values.iter().map(|v| with_selection(v, &raw)).collect();
    Ok(Json(Value::Array(projected)))
}

/// Fetch one caller-owned custom plugin.
///
/// # Errors
///
/// Returns a `400` problem for a malformed id and a `404` when the plugin is
/// not in the calling tenant's scope.
pub async fn get_plugin(
    Extension(ctx): Extension<SecurityContext>,
    Extension(control): Extension<Arc<ControlPlaneService>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, OagwProblem> {
    let tenant = ctx.subject_tenant_id();
    let uuid = gts::parse_resource_id(&id)
        .ok_or_else(|| bad_request(format!("invalid plugin id {id:?}")))?;
    let record = control
        .get_plugin(tenant, uuid)
        .await
        .map_err(domain_error_to_problem)?;
    Ok(Json(to_value(&record.entity)?))
}

/// Fetch the Starlark source of a caller-owned custom plugin.
///
/// # Errors
///
/// Returns a `400` problem for a malformed id and a `404` when the plugin is
/// not in the calling tenant's scope.
pub async fn get_plugin_source(
    Extension(ctx): Extension<SecurityContext>,
    Extension(control): Extension<Arc<ControlPlaneService>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, OagwProblem> {
    let tenant = ctx.subject_tenant_id();
    let uuid = gts::parse_resource_id(&id)
        .ok_or_else(|| bad_request(format!("invalid plugin id {id:?}")))?;
    let record = control
        .get_plugin(tenant, uuid)
        .await
        .map_err(domain_error_to_problem)?;
    Ok((
        StatusCode::OK,
        [("content-type", "text/plain; charset=utf-8")],
        record.entity.source_code,
    ))
}

/// Create a custom (Starlark) plugin.
///
/// # Errors
///
/// Returns a `400` problem for validation failures.
pub async fn create_plugin(
    Extension(ctx): Extension<SecurityContext>,
    Extension(control): Extension<Arc<ControlPlaneService>>,
    uri: Uri,
    body: Result<Json<Plugin>, JsonRejection>,
) -> Result<impl IntoResponse, OagwProblem> {
    let tenant = ctx.subject_tenant_id();
    let Json(input) = body.map_err(json_rejection_problem)?;
    let record = control
        .create_plugin(tenant, input)
        .await
        .map_err(domain_error_to_problem)?;
    let new_id = record.entity.id.as_deref().unwrap_or_default();
    Ok(created_json(to_value(&record.entity)?, &uri, new_id))
}

/// Delete an unlinked custom plugin.
///
/// # Errors
///
/// Returns a `404` problem when the plugin is not in scope and a `409` when
/// it is still referenced by an upstream or route.
pub async fn delete_plugin(
    Extension(ctx): Extension<SecurityContext>,
    Extension(control): Extension<Arc<ControlPlaneService>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, OagwProblem> {
    let tenant = ctx.subject_tenant_id();
    let uuid = gts::parse_resource_id(&id)
        .ok_or_else(|| bad_request(format!("invalid plugin id {id:?}")))?;
    control
        .delete_plugin(tenant, uuid)
        .await
        .map_err(domain_error_to_problem)?;
    Ok(no_content())
}

// ===========================================================================
// Proxy
// ===========================================================================

/// Proxy a request to the upstream resolved for `alias` (DESIGN §3.5).
///
/// # Errors
///
/// Returns a gateway problem (RFC 9457) for every data-plane failure; see
/// [`proxy_error_to_problem`] for the mapping.
pub async fn proxy(
    Extension(ctx): Extension<SecurityContext>,
    Extension(data_plane): Extension<Arc<DataPlaneService>>,
    Path((alias, rest)): Path<(String, String)>,
    req: Request<Body>,
) -> Result<Response<Body>, OagwProblem> {
    let response = data_plane
        .proxy(&ctx, req, alias, &rest)
        .await
        .map_err(proxy_error_to_problem)?;
    Ok(response)
}

/// Answer CORS preflight for the proxy route with a permissive 204 (no
/// upstream resolution, no tenant context — ADR-0004).
pub async fn proxy_preflight(req: Request<Body>) -> Response<Body> {
    let (parts, _body) = req.into_parts();
    let dummy = http::Request::from_parts(parts, ());
    crate::infra::cors::preflight_response(&dummy)
}
