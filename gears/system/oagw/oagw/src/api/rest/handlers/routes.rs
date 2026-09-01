//! Route management handlers (`/api/oagw/v1/routes`).

use std::sync::Arc;

use axum::Extension;
use axum::extract::{Path, RawQuery};
use axum::http::Uri;
use axum::response::{IntoResponse, Response};
use toolkit::api::canonical_prelude::{Json, created_json, no_content};
use toolkit_security::context::SecurityContext;

use super::common;
use crate::api::rest::dto::{
    CreateRouteRequest, PluginsConfigDto, ReplaceRouteRequest, RouteDto,
};
use crate::api::rest::query::ListQuery;
use crate::domain::error::DomainError;
use crate::domain::models::{ROUTE_TYPE, Route};
use crate::domain::service::{ControlPlaneService, RouteDraft, RouteUpdate};

/// Service extension type shared by every handler.
pub type Service = Arc<ControlPlaneService>;

/// `POST /routes` — 201 with the created route.
pub async fn create_route(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Service>,
    axum::Json(request): axum::Json<CreateRouteRequest>,
) -> Result<Response, DomainError> {
    let tenant_id = ctx.subject_tenant_id();
    let draft = RouteDraft {
        upstream_id: request.upstream_id,
        enabled: request.enabled,
        priority: request.priority,
        match_config: request.match_config,
        plugins: request.plugins.map(PluginsConfigDto::into_domain),
        rate_limit: request.rate_limit,
        tags: request.tags,
    };
    let route = service.create_route(tenant_id, draft).await?;
    let id = common::resource_id(ROUTE_TYPE, route.id);
    Ok(created_json(RouteDto::from(route), &uri, &id).into_response())
}

/// `GET /routes`
pub async fn list_routes(
    RawQuery(query): RawQuery,
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Service>,
) -> Result<Json<toolkit_odata::Page<serde_json::Value>>, DomainError> {
    let tenant_id = ctx.subject_tenant_id();
    let query = ListQuery::parse(query.as_deref())?;
    let items = service.list_routes(tenant_id).await?;
    common::paged(&items, &query, |route| {
        serde_json::to_value(RouteDto::from(route.clone())).unwrap_or(serde_json::Value::Null)
    })
}

/// `GET /routes/{id}`
pub async fn get_route(
    Path(raw_id): Path<String>,
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Service>,
) -> Result<Json<RouteDto>, DomainError> {
    let tenant_id = ctx.subject_tenant_id();
    let id = common::parse_resource_id(ROUTE_TYPE, &raw_id)?;
    let route = service.get_route(tenant_id, id).await?;
    Ok(Json(RouteDto::from(route)))
}

/// `PUT /routes/{id}` — `upstream_id` is immutable.
pub async fn replace_route(
    Path(raw_id): Path<String>,
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Service>,
    axum::Json(request): axum::Json<ReplaceRouteRequest>,
) -> Result<Json<RouteDto>, DomainError> {
    let tenant_id = ctx.subject_tenant_id();
    let id = common::parse_resource_id(ROUTE_TYPE, &raw_id)?;
    let update = RouteUpdate {
        enabled: request.enabled,
        priority: request.priority,
        match_config: request.match_config,
        plugins: request.plugins.map(PluginsConfigDto::into_domain),
        rate_limit: request.rate_limit,
        tags: request.tags,
    };
    let route = service.replace_route(tenant_id, id, update).await?;
    Ok(Json(RouteDto::from(route)))
}

/// `DELETE /routes/{id}`
pub async fn delete_route(
    Path(raw_id): Path<String>,
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Service>,
) -> Result<impl IntoResponse, DomainError> {
    let tenant_id = ctx.subject_tenant_id();
    let id = common::parse_resource_id(ROUTE_TYPE, &raw_id)?;
    service.delete_route(tenant_id, id).await?;
    Ok(no_content())
}

/// Serializes a route (list projection helper).
#[must_use]
pub fn route_json(route: &Route) -> serde_json::Value {
    serde_json::to_value(RouteDto::from(route.clone())).unwrap_or(serde_json::Value::Null)
}
