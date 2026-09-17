//! Upstream management handlers.

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
use crate::domain::models::{Upstream, UpstreamSpec};
use crate::gear::OagwState;

type SharedState = Extension<Arc<OagwState>>;

/// Creates an upstream (201).
#[tracing::instrument(skip(state, ctx, body), fields(request_id = Empty))]
pub async fn create_upstream(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    state: SharedState,
    Json(body): Json<UpstreamSpec>,
) -> ApiResult<impl IntoResponse> {
    let tenant_id = caller_tenant(&ctx);
    let upstream = state.upstreams.create(tenant_id, body)?;
    let id = upstream.id.to_string();
    Ok(created_json(upstream, &uri, &id).into_response())
}

/// Lists the caller's upstreams (200).
#[tracing::instrument(skip(state, ctx, params), fields(request_id = Empty))]
pub async fn list_upstreams(
    Extension(ctx): Extension<SecurityContext>,
    state: SharedState,
    Query(params): Query<ListParams>,
) -> ApiResult<Json<Vec<Upstream>>> {
    let query = params.to_list_query()?;
    Ok(Json(state.upstreams.list(caller_tenant(&ctx), &query)?))
}

/// Reads one upstream (200).
#[tracing::instrument(skip(state, ctx), fields(request_id = Empty))]
pub async fn get_upstream(
    Extension(ctx): Extension<SecurityContext>,
    state: SharedState,
    Path(id): Path<String>,
) -> ApiResult<Json<Upstream>> {
    let id = parse_resource_id(&id)?;
    Ok(Json(state.upstreams.get(caller_tenant(&ctx), id)?))
}

/// Replaces an upstream (200).
#[tracing::instrument(skip(state, ctx, body), fields(request_id = Empty))]
pub async fn replace_upstream(
    Extension(ctx): Extension<SecurityContext>,
    state: SharedState,
    Path(id): Path<String>,
    Json(body): Json<UpstreamSpec>,
) -> ApiResult<Json<Upstream>> {
    let id = parse_resource_id(&id)?;
    let upstream = state.upstreams.replace(caller_tenant(&ctx), id, body)?;
    Ok(Json(upstream))
}

/// Deletes an upstream together with its routes (204).
#[tracing::instrument(skip(state, ctx), fields(request_id = Empty))]
pub async fn delete_upstream(
    Extension(ctx): Extension<SecurityContext>,
    state: SharedState,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let id = parse_resource_id(&id)?;
    state.upstreams.delete(caller_tenant(&ctx), id)?;
    Ok(no_content())
}
