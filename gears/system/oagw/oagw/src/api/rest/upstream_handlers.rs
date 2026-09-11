//! REST handlers of the upstream management surface (FEATURE entry 2.2, the
//! five flows of `cpt-cf-oagw-feat-upstream-management`).
//!
//! Every handler does exactly three things before the service call: it
//! resolves the caller identity out of the platform security context, rejects
//! a missing or unresolvable context with the `401` OAGW authentication
//! surface, and hands over the body the caller sent. The permission gate and
//! the tenant scoping live in the domain service, so a handler cannot skip
//! them.
//!
//! Inbound bearer authentication itself is the api-gateway's job
//! (`require_auth_by_default`); the handler only reads the resolved context.

use std::sync::Arc;

use axum::extract::Extension;
use axum::http::{StatusCode, Uri};
use axum::Json;
use toolkit_security::SecurityContext;

use super::dto::{UpstreamListResponse, UpstreamRequest, UpstreamResponse};
use super::error::{ApiError, ApiResult, unauthenticated};
use crate::domain::list_query::ListQuery;
use crate::domain::services::management::{Actor, UpstreamManagement, UpstreamManagementService};

/// The caller identity of a management request.
///
/// `None` when the platform did not resolve a security context: no bearer
/// token, an invalid one, or no resolvable tenant. The rejection is the `401`
/// OAGW authentication surface, raised before any payload validation.
pub(crate) fn actor_of(security: Option<Extension<SecurityContext>>) -> Result<Actor, ApiError> {
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
/// resolvable context is the `401` surface and never a `422` from a
/// extractor-level rejection (`inst-um-cr-15`).
///
/// A body that is not the upstream write shape is a `400` validation failure
/// naming the offending field.
fn body_of(bytes: &axum::body::Bytes) -> Result<UpstreamRequest, ApiError> {
    serde_json::from_slice::<UpstreamRequest>(bytes).map_err(|error| {
        ApiError::Domain(crate::domain::error::DomainError::field_rejection(
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

/// `POST /oagw/v1/upstreams` — create an upstream
/// (`inst-um-cr-1` .. `-18`).
///
/// # Errors
///
/// `401` without a resolvable context, `403` on a deny, `400` on a shape or
/// validation failure, `409` on a same-tenant alias collision.
pub async fn create(
    security: Option<Extension<SecurityContext>>,
    Extension(service): Extension<Arc<UpstreamManagementService>>,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, [(axum::http::HeaderName, String); 1], Json<UpstreamResponse>)> {
    let actor = actor_of(security)?;
    let body = body_of(&body)?;
    let view = service.create(actor, body.into_upstream()).await?;
    // @cpt-begin:cpt-cf-oagw-flow-upstream-management-create:p1:inst-um-cr-14
    // `inst-um-cr-14`: `201 Created` with the stored record, its identifier,
    // its normalized alias and the applied scalar defaults, addressed by the
    // `Location` header.
    let location = format!("{}/{}", super::UPSTREAMS_PATH, view.record.id);
    let status = StatusCode::CREATED;
    Ok((status, [(axum::http::header::LOCATION, location)], Json(UpstreamResponse::from_record(&view.record, view.effective))))
    // @cpt-end:cpt-cf-oagw-flow-upstream-management-create:p1:inst-um-cr-14
}

// @cpt-begin:cpt-cf-oagw-flow-upstream-management-create:p1:inst-um-cr-10
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-create:p1:inst-um-cr-12
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-create:p1:inst-um-cr-13
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-create:p1:inst-um-cr-17
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-create:p1:inst-um-cr-18
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-create:p1:inst-um-cr-3
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-create:p1:inst-um-cr-4
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-create:p1:inst-um-cr-5
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-create:p1:inst-um-cr-7
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-create:p1:inst-um-cr-8
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-create:p1:inst-um-cr-9
/// `GET /oagw/v1/upstreams` — list the caller's own upstreams
/// (`inst-um-ls-1` .. `-8`).
///
/// # Errors
///
/// `401` on a missing context, `403` on a deny, `400` naming the offending
/// query parameter.
pub async fn list(
    security: Option<Extension<SecurityContext>>,
    Extension(service): Extension<Arc<UpstreamManagementService>>,
    uri: Uri,
) -> ApiResult<Json<UpstreamListResponse>> {
    let actor = actor_of(security)?;
    let query = ListQuery::parse(&query_pairs(&uri))?;
    let records = service.list(actor, &query).await?;
    // @cpt-begin:cpt-cf-oagw-flow-upstream-management-list:p1:inst-um-ls-7
    // `inst-um-ls-7`: the projected list carries the count of the records
    // actually returned, which is `0` for an empty result.
    let items: Vec<serde_json::Value> = records
        .iter()
        .map(|record| toolkit::api::select::apply_select(UpstreamResponse::from_record(record, crate::domain::services::management::EffectiveEnablement::own(record.enabled)), Some(query.select.as_slice())))
        .collect();
    Ok(Json(UpstreamListResponse { count: items.len(), items }))
    // @cpt-end:cpt-cf-oagw-flow-upstream-management-list:p1:inst-um-ls-7
}
//
// @cpt-end:cpt-cf-oagw-flow-upstream-management-create:p1:inst-um-cr-9
// @cpt-end:cpt-cf-oagw-flow-upstream-management-create:p1:inst-um-cr-8
// @cpt-end:cpt-cf-oagw-flow-upstream-management-create:p1:inst-um-cr-7
// @cpt-end:cpt-cf-oagw-flow-upstream-management-create:p1:inst-um-cr-5
// @cpt-end:cpt-cf-oagw-flow-upstream-management-create:p1:inst-um-cr-4
// @cpt-end:cpt-cf-oagw-flow-upstream-management-create:p1:inst-um-cr-3
// @cpt-end:cpt-cf-oagw-flow-upstream-management-create:p1:inst-um-cr-18
// @cpt-end:cpt-cf-oagw-flow-upstream-management-create:p1:inst-um-cr-17
// @cpt-end:cpt-cf-oagw-flow-upstream-management-create:p1:inst-um-cr-13
// @cpt-end:cpt-cf-oagw-flow-upstream-management-create:p1:inst-um-cr-12
// @cpt-end:cpt-cf-oagw-flow-upstream-management-create:p1:inst-um-cr-10
//

// @cpt-begin:cpt-cf-oagw-flow-upstream-management-list:p1:inst-um-ls-2
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-list:p1:inst-um-ls-3
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-list:p1:inst-um-ls-4
// @cpt-begin:cpt-cf-oagw-flow-upstream-management-list:p1:inst-um-ls-5
/// `GET /oagw/v1/upstreams/{id}` — read one upstream
//
// @cpt-end:cpt-cf-oagw-flow-upstream-management-list:p1:inst-um-ls-5
// @cpt-end:cpt-cf-oagw-flow-upstream-management-list:p1:inst-um-ls-4
// @cpt-end:cpt-cf-oagw-flow-upstream-management-list:p1:inst-um-ls-3
// @cpt-end:cpt-cf-oagw-flow-upstream-management-list:p1:inst-um-ls-2
//
/// (`inst-um-gt-1` .. `-6`).
///
/// # Errors
///
/// `401` on a missing context, `403` on a deny, `404` for a missing or
/// foreign identifier.
pub async fn get(
    security: Option<Extension<SecurityContext>>,
    Extension(service): Extension<Arc<UpstreamManagementService>>,
    axum::extract::Path(id): axum::extract::Path<uuid::Uuid>,
) -> ApiResult<Json<UpstreamResponse>> {
    let actor = actor_of(security)?;
    let view = service.get(actor, id).await?;
    Ok(Json(UpstreamResponse::from_record(&view.record, view.effective)))
}

/// `PUT /oagw/v1/upstreams/{id}` — full replacement, and therefore the enable
/// and disable path (`inst-um-rp-1` .. `-15`, `inst-um-en-1` .. `-14`).
///
/// # Errors
///
/// `401` on a missing context, `403` on a deny or an absent
/// `oagw:upstream:bind`, `400` on a shape or validation failure, `404` for a
/// missing or foreign identifier, `409` on an alias conflict.
pub async fn replace(
    security: Option<Extension<SecurityContext>>,
    Extension(service): Extension<Arc<UpstreamManagementService>>,
    axum::extract::Path(id): axum::extract::Path<uuid::Uuid>,
    body: axum::body::Bytes,
) -> ApiResult<Json<UpstreamResponse>> {
    let actor = actor_of(security)?;
    let body = body_of(&body)?;
    let view = service.replace(actor, id, body.into_upstream()).await?;
    Ok(Json(UpstreamResponse::from_record(&view.record, view.effective)))
}

/// `DELETE /oagw/v1/upstreams/{id}` — delete an upstream and its dependents
/// (`inst-um-dl-1` .. `-9`).
///
/// # Errors
///
/// `401` on a missing context, `403` on a deny, `404` for a missing or foreign
/// identifier.
pub async fn delete(
    security: Option<Extension<SecurityContext>>,
    Extension(service): Extension<Arc<UpstreamManagementService>>,
    axum::extract::Path(id): axum::extract::Path<uuid::Uuid>,
) -> ApiResult<StatusCode> {
    let actor = actor_of(security)?;
    service.delete(actor, id).await?;
    // @cpt-begin:cpt-cf-oagw-flow-upstream-management-delete:p1:inst-um-dl-6
    // `inst-um-dl-6`: `204 No Content` is the deletion confirmation and the
    // response carries no body.
    Ok(StatusCode::NO_CONTENT)
    // @cpt-end:cpt-cf-oagw-flow-upstream-management-delete:p1:inst-um-dl-6
}
