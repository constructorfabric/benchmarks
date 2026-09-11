// @cpt-begin:cpt-cf-oagw-dod-route-api-crud:p1:inst-route-handlers
//! Route management handlers.

use crate::api::rest::dto::{ListQuery, RouteCreateDto, RouteDto, RouteListDto, RouteReplaceDto};
use crate::api::rest::error::set_error_source;
use crate::api::rest::handlers::upstreams::{parse_id, validate_list_query};
use crate::api::rest::state::OagwState;
use crate::domain::error::{DomainError, ErrorSource};
use crate::domain::model::{Route, gts_resource_id};
use crate::domain::validate::validate_route;
use axum::extract::{Extension, Path, Query};
use axum::response::{IntoResponse, Response};
use http::StatusCode;
use std::sync::Arc;
use toolkit_security::SecurityContext;
use uuid::Uuid;

fn to_dto(route: Route) -> RouteDto {
    RouteDto {
        id: gts_resource_id("route", route.id),
        uuid: route.id,
        upstream_id: route.upstream_id,
        enabled: route.enabled,
        match_config: route.match_config,
        tags: route.tags,
        plugins: route.plugins,
        rate_limit: route.rate_limit,
        cors: route.cors,
    }
}

fn json_ok<T: serde::Serialize>(status: StatusCode, body: &T) -> Response {
    let mut response = (status, axum::Json(body)).into_response();
    set_error_source(&mut response, ErrorSource::Gateway);
    response
}

/// Create a route.
///
/// # Errors
/// Returns a validation error when the body or the upstream reference is
/// invalid, and a conflict on a duplicate match rule.
pub async fn create(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    axum::Json(body): axum::Json<RouteCreateDto>,
) -> Result<Response, DomainError> {
    let tenant_id = ctx.subject_tenant_id();
    // The upstream must exist and belong to the calling tenant; an ancestor
    // upstream is not directly addressable.
    if state
        .store
        .get_upstream(tenant_id, body.upstream_id)
        .is_none()
    {
        return Err(DomainError::validation(format!(
            "upstream_id `{}` does not name an upstream of this tenant",
            body.upstream_id
        )));
    }
    let route = Route {
        id: Uuid::new_v4(),
        tenant_id,
        upstream_id: body.upstream_id,
        enabled: body.enabled,
        match_config: body.match_config,
        tags: body.tags,
        plugins: body.plugins,
        rate_limit: body.rate_limit,
        cors: body.cors,
    };
    validate_route(&route)?;
    let created = state.store.create_route(route)?;
    Ok(json_ok(StatusCode::CREATED, &to_dto(created)))
}

/// List the tenant's routes.
///
/// # Errors
/// Returns a validation error when a query parameter is malformed.
pub async fn list(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    Query(query): Query<ListQuery>,
) -> Result<Response, DomainError> {
    validate_list_query(&query)?;
    let mut items: Vec<RouteDto> = state
        .store
        .list_routes(ctx.subject_tenant_id())
        .into_iter()
        .map(to_dto)
        .collect();
    items.sort_by_key(|r| r.uuid);
    let page: Vec<RouteDto> = items
        .into_iter()
        .skip(query.effective_skip())
        .take(query.effective_top())
        .collect();
    let count = page.len();
    Ok(json_ok(
        StatusCode::OK,
        &RouteListDto { items: page, count },
    ))
}

/// Fetch one route.
///
/// # Errors
/// Returns not-found when the tenant does not own a route with that id.
pub async fn get(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    Path(id): Path<String>,
) -> Result<Response, DomainError> {
    let id = parse_id(&id)?;
    let found = state
        .store
        .get_route(ctx.subject_tenant_id(), id)
        .ok_or_else(|| DomainError::not_found("route not found"))?;
    Ok(json_ok(StatusCode::OK, &to_dto(found)))
}

/// Replace a route. The upstream reference is immutable.
///
/// # Errors
/// Returns not-found when the route does not exist for the tenant, and a
/// conflict on a duplicate match rule.
pub async fn replace(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    Path(id): Path<String>,
    axum::Json(body): axum::Json<RouteReplaceDto>,
) -> Result<Response, DomainError> {
    let id = parse_id(&id)?;
    let tenant_id = ctx.subject_tenant_id();
    let existing = state
        .store
        .get_route(tenant_id, id)
        .ok_or_else(|| DomainError::not_found("route not found"))?;
    let replacement = Route {
        id,
        tenant_id,
        // Immutable: the replacement keeps the original upstream.
        upstream_id: existing.upstream_id,
        enabled: body.enabled,
        match_config: body.match_config,
        tags: body.tags,
        plugins: body.plugins,
        rate_limit: body.rate_limit,
        cors: body.cors,
    };
    validate_route(&replacement)?;
    let stored = state.store.replace_route(replacement)?;
    Ok(json_ok(StatusCode::OK, &to_dto(stored)))
}

/// Delete a route.
///
/// # Errors
/// Returns not-found when the tenant does not own a route with that id.
pub async fn delete(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    Path(id): Path<String>,
) -> Result<Response, DomainError> {
    let id = parse_id(&id)?;
    if state.store.delete_route(ctx.subject_tenant_id(), id) {
        let mut response = StatusCode::NO_CONTENT.into_response();
        set_error_source(&mut response, ErrorSource::Gateway);
        Ok(response)
    } else {
        Err(DomainError::not_found("route not found"))
    }
}
// @cpt-end:cpt-cf-oagw-dod-route-api-crud:p1:inst-route-handlers
