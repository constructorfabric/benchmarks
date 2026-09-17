//! Route management handlers.

use std::sync::Arc;

use axum::Extension;
use axum::extract::{Path, Query};
use axum::http::Uri;
use toolkit::api::canonical_prelude::*;
use toolkit_security::SecurityContext;
use tracing::field::Empty;

use super::caller_tenant;
use crate::api::rest::dto::ListParams;
use crate::api::rest::extractors::parse_resource_id;
use crate::domain::models::{Route, RouteSpec, RouteUpdate};
use crate::gear::OagwState;

type SharedState = Extension<Arc<OagwState>>;

/// Creates a route (201).
#[tracing::instrument(skip(state, ctx, body), fields(request_id = Empty))]
pub async fn create_route(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    state: SharedState,
    Json(body): Json<RouteSpec>,
) -> ApiResult<impl IntoResponse> {
    let tenant_id = caller_tenant(&ctx);
    let route = state.routes.create(tenant_id, body)?;
    let id = route.id.to_string();
    Ok(created_json(route, &uri, &id).into_response())
}

/// Lists the caller's routes (200).
#[tracing::instrument(skip(state, ctx, params), fields(request_id = Empty))]
pub async fn list_routes(
    Extension(ctx): Extension<SecurityContext>,
    state: SharedState,
    Query(params): Query<ListParams>,
) -> ApiResult<Json<Vec<Route>>> {
    let query = params.to_list_query()?;
    Ok(Json(state.routes.list(caller_tenant(&ctx), &query)?))
}

/// Reads one route (200).
#[tracing::instrument(skip(state, ctx), fields(request_id = Empty))]
pub async fn get_route(
    Extension(ctx): Extension<SecurityContext>,
    state: SharedState,
    Path(id): Path<String>,
) -> ApiResult<Json<Route>> {
    let id = parse_resource_id(&id)?;
    Ok(Json(state.routes.get(caller_tenant(&ctx), id)?))
}

/// Replaces a route; `upstream_id` is immutable (200).
#[tracing::instrument(skip(state, ctx, body), fields(request_id = Empty))]
pub async fn replace_route(
    Extension(ctx): Extension<SecurityContext>,
    state: SharedState,
    Path(id): Path<String>,
    Json(body): Json<RouteUpdate>,
) -> ApiResult<Json<Route>> {
    let id = parse_resource_id(&id)?;
    let route = state.routes.replace(caller_tenant(&ctx), id, body)?;
    Ok(Json(route))
}

/// Deletes a route (204).
#[tracing::instrument(skip(state, ctx), fields(request_id = Empty))]
pub async fn delete_route(
    Extension(ctx): Extension<SecurityContext>,
    state: SharedState,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let id = parse_resource_id(&id)?;
    state.routes.delete(caller_tenant(&ctx), id)?;
    Ok(no_content())
}
