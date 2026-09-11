//! REST handlers of the route management surface (FEATURE entry 2.3, the five
//! flows of `cpt-cf-oagw-feat-route-management`).
//!
//! Every handler does exactly three things before the service call: it
//! resolves the caller identity out of the platform security context, rejects
//! a missing or unresolvable context with the `401` OAGW authentication
//! surface, and hands over the body the caller sent. The permission gate, the
//! upstream reference resolution, the match-block validation, the binding-time
//! plugin resolvability and the tenant scoping all live in the domain service,
//! so a handler cannot skip them.
//!
//! Inbound bearer authentication itself is the api-gateway's job
//! (`require_auth_by_default`); the handler only reads the resolved context.

use std::sync::Arc;

use axum::extract::Extension;
use axum::http::{StatusCode, Uri};
use axum::Json;
use toolkit_security::SecurityContext;

use super::dto::{RouteListResponse, RouteRequest, RouteResponse, RouteUpdateRequest};
use super::error::{ApiError, ApiResult, unauthenticated};
use crate::domain::error::DomainError;
use crate::domain::gts_helpers::uuid_of;
use crate::domain::list_query::ListQuery;
use crate::domain::services::management::Actor;
use crate::domain::services::route_management::{RouteManagement, RouteManagementService};

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
/// A body that is not the route write shape is a `400` validation failure
/// naming the offending field.
fn body_of<T>(bytes: &axum::body::Bytes) -> Result<T, ApiError>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_slice::<T>(bytes).map_err(|error| {
        ApiError::Domain(DomainError::field_rejection(
            "body",
            &format!("failed to deserialize the JSON body into the target type: {error}"),
        ))
    })
}

/// Parse the replacement body, rejecting a supplied `upstream_id` first
/// (`inst-rm-replace-9`).
///
/// `upstream_id` is immutable and is not part of the update DTO, so the body is
/// read as JSON once to detect the field, and the rejection names the field
/// regardless of the supplied value. A body without it is decoded into the
/// update DTO, which is closed to unknown properties and would otherwise reject
/// the field with a less specific message.
fn update_body_of(bytes: &axum::body::Bytes) -> Result<RouteUpdateRequest, ApiError> {
    let value: serde_json::Value = body_of(bytes)?;
    if value.get("upstream_id").is_some() {
        return Err(ApiError::Domain(DomainError::field_rejection(
            "upstream_id",
            "upstream_id is immutable and is not part of the replacement payload",
        )));
    }
    serde_json::from_value(value).map_err(|error| {
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

/// The route identifier a by-identifier path names.
///
/// A bare UUID is the form the create response reports; the anonymous GTS
/// resource instance identifier `gts.cf.core.oagw.route.v1~{uuid}` is accepted
/// as well, because it is the resource form the path parameter resolves under
/// (`cpt-cf-oagw-actor-types-registry`). Anything else is a `400` validation
/// failure naming the field, raised through the shared problem+json surface
/// instead of an extractor-level plain-text rejection.
fn route_id_of(segment: &str) -> Result<uuid::Uuid, ApiError> {
    let parsed = uuid::Uuid::parse_str(segment).or_else(|_| uuid_of(segment).ok_or(()));
    parsed.map_err(|_| {
        ApiError::Domain(DomainError::field_rejection(
            "id",
            "the route identifier must be a UUID or an anonymous GTS resource identifier",
        ))
    })
}

/// `POST /oagw/v1/routes` — create a route.
///
/// # Errors
///
/// `401` without a resolvable context, `403` on a deny, `400` on a shape,
/// match-block or unresolvable-reference failure, `409` on a match-rule
/// collision.
pub async fn create(
    security: Option<Extension<SecurityContext>>,
    Extension(service): Extension<Arc<RouteManagementService>>,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, [(axum::http::HeaderName, String); 1], Json<RouteResponse>)> {
    let actor = actor_of(security)?;
    let body: RouteRequest = body_of(&body)?;
    let record = service.create(actor, body.into_route()).await?;
    // @cpt-begin:cpt-cf-oagw-flow-route-management-create-route:p1:inst-rm-create-12
    // `inst-rm-create-12`: `201 Created` with the stored route, including the
    // `enabled` default `true` and the `priority` default `0` when either was
    // omitted, addressed by the `Location` header.
    let location = format!("{}/{}", super::ROUTES_PATH, record.id);
    let status = StatusCode::CREATED;
    Ok((status, [(axum::http::header::LOCATION, location)], Json(RouteResponse::from_record(&record))))
    // @cpt-end:cpt-cf-oagw-flow-route-management-create-route:p1:inst-rm-create-12
}

// @cpt-begin:cpt-cf-oagw-flow-route-management-create-route:p1:inst-rm-create-10
// @cpt-begin:cpt-cf-oagw-flow-route-management-create-route:p1:inst-rm-create-11
// @cpt-begin:cpt-cf-oagw-flow-route-management-create-route:p1:inst-rm-create-11b
// @cpt-begin:cpt-cf-oagw-flow-route-management-create-route:p1:inst-rm-create-14
// @cpt-begin:cpt-cf-oagw-flow-route-management-create-route:p1:inst-rm-create-3
// @cpt-begin:cpt-cf-oagw-flow-route-management-create-route:p1:inst-rm-create-4
// @cpt-begin:cpt-cf-oagw-flow-route-management-create-route:p1:inst-rm-create-5
// @cpt-begin:cpt-cf-oagw-flow-route-management-create-route:p1:inst-rm-create-6
// @cpt-begin:cpt-cf-oagw-flow-route-management-create-route:p1:inst-rm-create-7
// @cpt-begin:cpt-cf-oagw-flow-route-management-create-route:p1:inst-rm-create-8
// @cpt-begin:cpt-cf-oagw-flow-route-management-create-route:p1:inst-rm-create-9
/// `GET /oagw/v1/routes` — list the caller's own routes
/// (`inst-rm-list-1` .. `-7`).
///
/// # Errors
///
/// `401` on a missing context, `403` on a deny, `400` naming the offending
/// query parameter.
pub async fn list(
    security: Option<Extension<SecurityContext>>,
    Extension(service): Extension<Arc<RouteManagementService>>,
    uri: Uri,
) -> ApiResult<Json<RouteListResponse>> {
    let actor = actor_of(security)?;
    // The same OData parser entry 2.2 delivered, over the route field set.
    let query = ListQuery::parse_route(&query_pairs(&uri))?;
    let records = service.list(actor, &query).await?;
    // @cpt-begin:cpt-cf-oagw-flow-route-management-list-routes:p1:inst-rm-list-7
    // `inst-rm-list-7`/`inst-rm-lq-3`: the returned records carry their match
    // blocks and overrides, `$select` projects them, and the count is the count
    // of the records actually returned.
    let items: Vec<serde_json::Value> = records
        .iter()
        .map(|record| toolkit::api::select::apply_select(RouteResponse::from_record(record), Some(query.select.as_slice())))
        .collect();
    Ok(Json(RouteListResponse { count: items.len(), items }))
    // @cpt-end:cpt-cf-oagw-flow-route-management-list-routes:p1:inst-rm-list-7
}
//
// @cpt-end:cpt-cf-oagw-flow-route-management-create-route:p1:inst-rm-create-9
// @cpt-end:cpt-cf-oagw-flow-route-management-create-route:p1:inst-rm-create-8
// @cpt-end:cpt-cf-oagw-flow-route-management-create-route:p1:inst-rm-create-7
// @cpt-end:cpt-cf-oagw-flow-route-management-create-route:p1:inst-rm-create-6
// @cpt-end:cpt-cf-oagw-flow-route-management-create-route:p1:inst-rm-create-5
// @cpt-end:cpt-cf-oagw-flow-route-management-create-route:p1:inst-rm-create-4
// @cpt-end:cpt-cf-oagw-flow-route-management-create-route:p1:inst-rm-create-3
// @cpt-end:cpt-cf-oagw-flow-route-management-create-route:p1:inst-rm-create-14
// @cpt-end:cpt-cf-oagw-flow-route-management-create-route:p1:inst-rm-create-11b
// @cpt-end:cpt-cf-oagw-flow-route-management-create-route:p1:inst-rm-create-11
// @cpt-end:cpt-cf-oagw-flow-route-management-create-route:p1:inst-rm-create-10
//

// @cpt-begin:cpt-cf-oagw-flow-route-management-list-routes:p1:inst-rm-list-2
// @cpt-begin:cpt-cf-oagw-flow-route-management-list-routes:p1:inst-rm-list-4
// @cpt-begin:cpt-cf-oagw-flow-route-management-list-routes:p1:inst-rm-list-5
// @cpt-begin:cpt-cf-oagw-flow-route-management-list-routes:p1:inst-rm-list-6
// @cpt-begin:cpt-cf-oagw-flow-route-management-list-routes:p1:inst-rm-list-8
/// `GET /oagw/v1/routes/{id}` — read one route (`inst-rm-list-3`).
//
// @cpt-end:cpt-cf-oagw-flow-route-management-list-routes:p1:inst-rm-list-8
// @cpt-end:cpt-cf-oagw-flow-route-management-list-routes:p1:inst-rm-list-6
// @cpt-end:cpt-cf-oagw-flow-route-management-list-routes:p1:inst-rm-list-5
// @cpt-end:cpt-cf-oagw-flow-route-management-list-routes:p1:inst-rm-list-4
// @cpt-end:cpt-cf-oagw-flow-route-management-list-routes:p1:inst-rm-list-2
//
///
/// # Errors
///
/// `401` on a missing context, `403` on a deny, `404` for a missing, foreign
/// or removed identifier.
pub async fn get(
    security: Option<Extension<SecurityContext>>,
    Extension(service): Extension<Arc<RouteManagementService>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> ApiResult<Json<RouteResponse>> {
    let actor = actor_of(security)?;
    let id = route_id_of(&id)?;
    let record = service.get(actor, id).await?;
    Ok(Json(RouteResponse::from_record(&record)))
}

/// `PUT /oagw/v1/routes/{id}` — full replacement, and therefore the enable and
/// disable path (`inst-rm-replace-1` .. `-11`, `inst-rm-enab-1` .. `-8`).
///
/// # Errors
///
/// `401` on a missing context, `403` on a deny, `400` on a shape or validation
/// failure or on a supplied `upstream_id`, `404` for a missing, foreign or
/// removed identifier, `409` on a match-rule collision.
pub async fn replace(
    security: Option<Extension<SecurityContext>>,
    Extension(service): Extension<Arc<RouteManagementService>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<RouteResponse>> {
    let actor = actor_of(security)?;
    let id = route_id_of(&id)?;
    let body = update_body_of(&body)?;
    let record = service.replace(actor, id, body.into_route()).await?;
    Ok(Json(RouteResponse::from_record(&record)))
}

/// `DELETE /oagw/v1/routes/{id}` — delete a route and its dependent rows
/// (`inst-rm-del-1` .. `-5`).
///
/// # Errors
///
/// `401` on a missing context, `403` on a deny, `404` for a missing, foreign
/// or removed identifier.
pub async fn delete(
    security: Option<Extension<SecurityContext>>,
    Extension(service): Extension<Arc<RouteManagementService>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> ApiResult<StatusCode> {
    let actor = actor_of(security)?;
    let id = route_id_of(&id)?;
    service.delete(actor, id).await?;
    // @cpt-begin:cpt-cf-oagw-flow-route-management-delete-route:p1:inst-rm-del-5
    // `inst-rm-del-5`: `204 No Content` is the deletion confirmation, the
    // response carries no body, and no queryable trace of the route is left.
    Ok(StatusCode::NO_CONTENT)
    // @cpt-end:cpt-cf-oagw-flow-route-management-delete-route:p1:inst-rm-del-5
}
