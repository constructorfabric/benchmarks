//! Management-API handlers plus the data-plane proxy entry point.
//!
//! Management handlers are thin: all tenant-scoping, validation, and conflict
//! semantics live in the control plane ([`crate::state`]). The proxy handler
//! hands off to the data plane ([`crate::proxy`]).

use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, Path, Query};
use http::StatusCode;
use toolkit::api::canonical_prelude::ApiResult;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::dto::{self, ListQuery, ListResponse};
use crate::model::{CustomPlugin, Route, Upstream};
use crate::state::OagwState;

// ---------------------------------------------------------------------------
// Upstreams
// ---------------------------------------------------------------------------

pub async fn create_upstream(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(sec): Extension<SecurityContext>,
    Json(input): Json<dto::UpstreamCreate>,
) -> ApiResult<(StatusCode, Json<Upstream>)> {
    let upstream = state.create_upstream(&sec, input)?;
    Ok((StatusCode::CREATED, Json(upstream)))
}

pub async fn list_upstreams(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(sec): Extension<SecurityContext>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<ListResponse<Upstream>>> {
    let mut items = state.list_upstreams(sec.subject_tenant_id());
    let mut total = items.len() as u64;

    if let Some(expr) = dto::parse_filter(query.filter.as_deref()) {
        items.retain(|u| match &expr {
            dto::FilterExpr::AliasEq(a) => &u.alias == a,
            dto::FilterExpr::EnabledEq(b) => u.enabled == *b,
            // upstreams have no `type` / `upstream_id` / `name` fields
            dto::FilterExpr::TypeEq(_)
            | dto::FilterExpr::UpstreamIdEq(_)
            | dto::FilterExpr::NameEq(_)
            | dto::FilterExpr::Unsupported => true,
        });
        total = items.len() as u64;
    }

    if let Some((field, dir)) = dto::parse_orderby(query.orderby.as_deref()) {
        items.sort_by(|a, b| {
            let ord = match field.as_str() {
                "alias" => a.alias.cmp(&b.alias),
                "id" => a.id.cmp(&b.id),
                _ => a.created_at.cmp(&b.created_at),
            };
            if dir == dto::SortDir::Desc {
                ord.reverse()
            } else {
                ord
            }
        });
    }

    let skip = query.offset() as usize;
    let top = query.limit() as usize;
    let next_cursor = if skip + top < total as usize {
        Some((skip + top).to_string())
    } else {
        None
    };
    let value: Vec<Upstream> = items.into_iter().skip(skip).take(top).collect();
    Ok(Json(ListResponse {
        value,
        total,
        next_cursor,
    }))
}

pub async fn get_upstream(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(sec): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Upstream>> {
    let upstream = state
        .get_upstream(sec.subject_tenant_id(), id)
        .ok_or_else(|| crate::error::OagwError::not_found_resource(id.to_string()))?;
    Ok(Json(upstream))
}

pub async fn put_upstream(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(sec): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
    Json(input): Json<dto::UpstreamCreate>,
) -> ApiResult<Json<Upstream>> {
    let upstream = state.put_upstream(&sec, id, input)?;
    Ok(Json(upstream))
}

pub async fn delete_upstream(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(sec): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    state.delete_upstream(&sec, id)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

pub async fn create_route(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(sec): Extension<SecurityContext>,
    Json(input): Json<dto::RouteCreate>,
) -> ApiResult<(StatusCode, Json<Route>)> {
    let route = state.create_route(&sec, input)?;
    Ok((StatusCode::CREATED, Json(route)))
}

pub async fn list_routes(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(sec): Extension<SecurityContext>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<ListResponse<Route>>> {
    let mut items = state.list_routes(sec.subject_tenant_id());
    let mut total = items.len() as u64;

    if let Some(expr) = dto::parse_filter(query.filter.as_deref()) {
        items.retain(|r| match &expr {
            dto::FilterExpr::UpstreamIdEq(u) => r.upstream_id == *u,
            dto::FilterExpr::TypeEq(_) | dto::FilterExpr::AliasEq(_) | dto::FilterExpr::NameEq(_)
            | dto::FilterExpr::EnabledEq(_) | dto::FilterExpr::Unsupported => true,
        });
        total = items.len() as u64;
    }

    if let Some((field, dir)) = dto::parse_orderby(query.orderby.as_deref()) {
        items.sort_by(|a, b| {
            let ord = match field.as_str() {
                "id" => a.id.cmp(&b.id),
                _ => a.created_at.cmp(&b.created_at),
            };
            if dir == dto::SortDir::Desc {
                ord.reverse()
            } else {
                ord
            }
        });
    }

    let skip = query.offset() as usize;
    let top = query.limit() as usize;
    let next_cursor = if skip + top < total as usize {
        Some((skip + top).to_string())
    } else {
        None
    };
    let value: Vec<Route> = items.into_iter().skip(skip).take(top).collect();
    Ok(Json(ListResponse {
        value,
        total,
        next_cursor,
    }))
}

pub async fn get_route(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(sec): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Route>> {
    let route = state
        .get_route(sec.subject_tenant_id(), id)
        .ok_or_else(|| crate::error::OagwError::not_found_resource(id.to_string()))?;
    Ok(Json(route))
}

pub async fn put_route(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(sec): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
    Json(input): Json<dto::RoutePut>,
) -> ApiResult<Json<Route>> {
    let route = state.put_route(&sec, id, input)?;
    Ok(Json(route))
}

pub async fn delete_route(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(sec): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    state.delete_route(&sec, id)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Custom plugins
// ---------------------------------------------------------------------------

pub async fn create_plugin(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(sec): Extension<SecurityContext>,
    Json(input): Json<dto::PluginCreate>,
) -> ApiResult<(StatusCode, Json<CustomPlugin>)> {
    let plugin = state.create_plugin(&sec, input)?;
    Ok((StatusCode::CREATED, Json(plugin)))
}

pub async fn list_plugins(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(sec): Extension<SecurityContext>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<ListResponse<CustomPlugin>>> {
    let mut items = state.list_plugins(sec.subject_tenant_id());
    let mut total = items.len() as u64;

    if let Some(expr) = dto::parse_filter(query.filter.as_deref()) {
        items.retain(|p| match &expr {
            dto::FilterExpr::NameEq(n) => &p.name == n,
            dto::FilterExpr::TypeEq(t) => &p.plugin_type == t,
            dto::FilterExpr::AliasEq(_) | dto::FilterExpr::UpstreamIdEq(_)
            | dto::FilterExpr::EnabledEq(_) | dto::FilterExpr::Unsupported => true,
        });
        total = items.len() as u64;
    }

    if let Some((field, dir)) = dto::parse_orderby(query.orderby.as_deref()) {
        items.sort_by(|a, b| {
            let ord = match field.as_str() {
                "name" => a.name.cmp(&b.name),
                "id" => a.id.cmp(&b.id),
                _ => a.created_at.cmp(&b.created_at),
            };
            if dir == dto::SortDir::Desc {
                ord.reverse()
            } else {
                ord
            }
        });
    }

    let skip = query.offset() as usize;
    let top = query.limit() as usize;
    let next_cursor = if skip + top < total as usize {
        Some((skip + top).to_string())
    } else {
        None
    };
    let value: Vec<CustomPlugin> = items.into_iter().skip(skip).take(top).collect();
    Ok(Json(ListResponse {
        value,
        total,
        next_cursor,
    }))
}

pub async fn get_plugin(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(sec): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<CustomPlugin>> {
    let plugin = state
        .get_plugin(sec.subject_tenant_id(), id)
        .ok_or_else(|| crate::error::OagwError::not_found_resource(id.to_string()))?;
    Ok(Json(plugin))
}

pub async fn delete_plugin(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(sec): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    state.delete_plugin(&sec, id)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Data plane
// ---------------------------------------------------------------------------

/// Proxy one request into the OAGW upstream graph.
///
/// The path template is `/oagw/v1/proxy/{alias}/{*rest}`; registration is
/// gear-relative, so api-gateway mounts these under its own prefix when one
/// is configured.
pub async fn proxy(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(sec): Extension<SecurityContext>,
    Path((alias, rest)): Path<(String, String)>,
    req: axum::extract::Request,
) -> axum::response::Response {
    let resp = crate::proxy::proxy_request(
        state,
        &sec,
        &alias.to_ascii_lowercase(),
        &rest,
        req,
    )
    .await;
    resp.into()
}
