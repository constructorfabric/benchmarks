//! REST handlers of the plugin management surface (FEATURE entry 2.6, the
//! four flows `plugin-create`/`plugin-read`/`plugin-source`/`plugin-delete`).
//!
//! Every handler does exactly three things before the service call: it
//! resolves the caller identity out of the platform security context, rejects
//! a missing or unresolvable context with the `401` OAGW authentication
//! surface, and hands over the body or identifier the caller sent. The
//! per-base-type permission gate, the base-type resolution, the
//! `(tenant_id, name)` uniqueness check, the reference scan and the tenant
//! scoping all live in the domain service, so a handler cannot skip them.
//!
//! There is **no** `PUT` or `PATCH` handler on the plugin path: a custom plugin
//! is immutable after creation
//! (`cpt-cf-oagw-dod-plugin-system-immutability-delete`), and an update is
//! performed by creating a new plugin and re-binding the references.
//!
//! Inbound bearer authentication itself is the api-gateway's job
//! (`require_auth_by_default`); the handler only reads the resolved context.

use std::sync::Arc;

use axum::extract::Extension;
use axum::http::{StatusCode, Uri};
use axum::Json;
use toolkit_security::SecurityContext;

use super::dto::{PluginListResponse, PluginRequest, PluginResponse, PluginSourceResponse};
use super::error::{ApiError, ApiResult, unauthenticated};
use crate::domain::error::DomainError;
use crate::domain::gts_helpers::uuid_of;
use crate::domain::list_query::ListQuery;
use crate::domain::services::management::Actor;
use crate::domain::services::plugin_management::{PluginManagement, PluginManagementService};

/// The caller identity of a management request.
///
/// `None` when the platform did not resolve a security context: no bearer
/// token, an invalid one, or no resolvable tenant. The rejection is the `401`
/// OAGW authentication surface, raised before any payload validation.
fn actor_of(security: Option<Extension<SecurityContext>>) -> Result<Actor, ApiError> {
    let Some(Extension(security)) = security else {
        return Err(unauthenticated());
    };
    let tenant_id = security.subject_tenant_id();
    if tenant_id.is_nil() {
        return Err(unauthenticated());
    }
    Ok(Actor { tenant_id, principal_id: security.subject_id() })
}

/// Parse the request body *after* the actor check, so a request with no
/// resolvable context is the `401` surface and never a `422` from an
/// extractor-level rejection.
///
/// A body that is not the plugin create shape is a `400` validation failure
/// naming the offending field.
fn body_of(bytes: &axum::body::Bytes) -> Result<PluginRequest, ApiError> {
    serde_json::from_slice::<PluginRequest>(bytes).map_err(|error| {
        ApiError::Domain(DomainError::field_rejection(
            "body",
            &format!("failed to deserialize the JSON body into the target type: {error}"),
        ))
    })
}

/// The query-string pairs of a list request, in the order the client wrote
/// them.
fn query_pairs(uri: &Uri) -> Vec<(String, String)> {
    let Some(query) = uri.query() else {
        return Vec::new();
    };
    form_urlencoded::parse(query.as_bytes())
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect()
}

/// The plugin identifier a by-identifier path names.
///
/// A bare UUID is the form the create response reports; the anonymous GTS
/// resource instance identifier `gts.cf.core.oagw.{type}_plugin.v1~{uuid}` is
/// accepted as well, because it is the resource form the path parameter
/// resolves under (`cpt-cf-oagw-actor-types-registry`). Anything else is a
/// `400` validation failure naming the field, raised through the shared
/// problem+json surface instead of an extractor-level plain-text rejection.
fn plugin_id_of(segment: &str) -> Result<uuid::Uuid, ApiError> {
    let parsed = uuid::Uuid::parse_str(segment).or_else(|_| uuid_of(segment).ok_or(()));
    parsed.map_err(|_| {
        ApiError::Domain(DomainError::field_rejection(
            "id",
            "the plugin identifier must be a UUID or an anonymous GTS resource identifier",
        ))
    })
}

/// `POST /oagw/v1/plugins` — create a custom plugin
/// (`inst-ps-create-1` .. `-9`).
///
/// # Errors
///
/// `401` without a resolvable context, `403` on a deny, `400` on a shape or
/// base-type failure, `409` when the `(tenant_id, name)` pair is taken.
pub async fn create(
    security: Option<Extension<SecurityContext>>,
    Extension(service): Extension<Arc<PluginManagementService>>,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, [(axum::http::HeaderName, String); 1], Json<PluginResponse>)> {
    let actor = actor_of(security)?;
    let body: PluginRequest = body_of(&body)?;
    let record = service.create(actor, body.into_plugin()).await?;
    // `inst-ps-create-9`: `201 Created` with the created record, addressed by
    // the `Location` header and carrying no secret material.
    let location = format!("{}/{}", super::PLUGINS_PATH, record.id);
    let status = StatusCode::CREATED;
    Ok((status, [(axum::http::header::LOCATION, location)], Json(PluginResponse::from_record(&record))))
}

/// `GET /oagw/v1/plugins` — list the caller's own plugins
/// (`inst-ps-read-1` .. `-7`).
///
/// # Errors
///
/// `401` on a missing context, `403` on a deny, `400` naming the offending
/// query parameter.
pub async fn list(
    security: Option<Extension<SecurityContext>>,
    Extension(service): Extension<Arc<PluginManagementService>>,
    uri: Uri,
) -> ApiResult<Json<PluginListResponse>> {
    let actor = actor_of(security)?;
    // The same OData parser entries 2.2 and 2.3 delivered, over the plugin
    // field set.
    let query = ListQuery::parse_plugin(&query_pairs(&uri))?;
    let records = service.list(actor, &query).await?;
    let items: Vec<serde_json::Value> = records
        .iter()
        .map(|record| toolkit::api::select::apply_select(PluginResponse::from_record(record), Some(query.select.as_slice())))
        .collect();
    Ok(Json(PluginListResponse { count: items.len(), items }))
}

/// `GET /oagw/v1/plugins/{id}` — read one plugin
/// (`cpt-cf-oagw-flow-plugin-system-plugin-read`).
///
/// # Errors
///
/// `401` on a missing context, `403` on a deny, `404` for a missing, foreign
/// or removed identifier.
pub async fn get(
    security: Option<Extension<SecurityContext>>,
    Extension(service): Extension<Arc<PluginManagementService>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> ApiResult<Json<PluginResponse>> {
    let actor = actor_of(security)?;
    let id = plugin_id_of(&id)?;
    let record = service.get(actor, id).await?;
    Ok(Json(PluginResponse::from_record(&record)))
}

/// `GET /oagw/v1/plugins/{id}/source` — the registered source content
/// (`inst-ps-source-1` .. `-5`).
///
/// # Errors
///
/// `401` on a missing context, `403` on a deny, `404` for a missing or foreign
/// identifier and for a named plugin, which the in-process registry resolves
/// and which carries no stored source.
pub async fn source(
    security: Option<Extension<SecurityContext>>,
    Extension(service): Extension<Arc<PluginManagementService>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> ApiResult<Json<PluginSourceResponse>> {
    let actor = actor_of(security)?;
    let id = plugin_id_of(&id)?;
    let record = service.source(actor, id).await?;
    // `inst-ps-source-5`: the source content as an opaque reference artifact.
    Ok(Json(PluginSourceResponse::from_record(&record)))
}

/// `DELETE /oagw/v1/plugins/{id}` — delete an unreferenced plugin
/// (`inst-ps-del-1` .. `-9`).
///
/// # Errors
///
/// `401` on a missing context, `403` on a deny, `404` for a missing, foreign
/// or removed identifier, `409 PluginInUse` with its `referenced_by` body.
pub async fn delete(
    security: Option<Extension<SecurityContext>>,
    Extension(service): Extension<Arc<PluginManagementService>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> ApiResult<StatusCode> {
    let actor = actor_of(security)?;
    let id = plugin_id_of(&id)?;
    service.delete(actor, id).await?;
    // `inst-ps-del-9`: `204 No Content` is the deletion confirmation, the
    // response carries no body, and no queryable trace of the plugin is left.
    Ok(StatusCode::NO_CONTENT)
}
