// Created: 2026-09-03 by Constructor Tech
//! Axum handlers of the OAGW REST surface.
//!
//! Every handler extracts the shared [`OagwState`] from the request
//! extensions together with the caller's `SecurityContext`, then delegates to
//! the control-plane services in [`crate::control`] or the data-plane
//! pipeline in [`crate::proxy`]. Errors render as RFC 9457 problem documents
//! through `OagwError: IntoResponse`, so handlers keep the gateway's own
//! error catalogue rather than the generic canonical one.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, Path, RawQuery};
use axum::http::StatusCode;
use axum::response::Response;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::dto::ListEnvelope;
use crate::error::{ErrorKind, OagwError};
use crate::state::OagwState;

type ApiResult<T> = Result<T, OagwError>;

/// Parses a path identifier, accepting both a bare UUID and a GTS identifier
/// whose instance part is a UUID.
fn resource_id(raw: &str, label: &str) -> Result<Uuid, OagwError> {
    if let Ok(id) = Uuid::parse_str(raw) {
        return Ok(id);
    }
    crate::gts::uuid_instance(raw).ok_or_else(|| {
        OagwError::new(
            ErrorKind::Validation,
            format!("{label} identifier '{raw}' is not a valid GTS identifier"),
        )
    })
}

// ---------------------------------------------------------------------------
// Upstreams
// ---------------------------------------------------------------------------

/// Creates an upstream.
pub async fn create_upstream(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(sec): Extension<SecurityContext>,
    crate::api::json::JsonBody(input): crate::api::json::JsonBody<crate::model::UpstreamInput>,
) -> ApiResult<(StatusCode, Json<crate::model::Upstream>)> {
    let upstream = crate::control::create_upstream(&state, &sec, input).await?;
    Ok((StatusCode::CREATED, Json(upstream)))
}

/// Lists the upstreams of the calling tenant.
pub async fn list_upstreams(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(sec): Extension<SecurityContext>,
    RawQuery(query): RawQuery,
) -> ApiResult<Json<ListEnvelope>> {
    let items = crate::control::list_upstreams(&state, &sec);
    let config = &state.config;
    let envelope = super::dto::list_page(items, query.as_deref(), config.list_top_default, config.list_top_max)?;
    Ok(Json(envelope))
}

/// Fetches an upstream.
pub async fn get_upstream(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(sec): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> ApiResult<Json<crate::model::Upstream>> {
    let id = resource_id(&id, "upstream")?;
    let upstream = crate::control::get_upstream(&state, &sec, id)?;
    Ok(Json(upstream))
}

/// Replaces an upstream.
pub async fn update_upstream(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(sec): Extension<SecurityContext>,
    Path(id): Path<String>,
    crate::api::json::JsonBody(input): crate::api::json::JsonBody<crate::model::UpstreamInput>,
) -> ApiResult<Json<crate::model::Upstream>> {
    let id = resource_id(&id, "upstream")?;
    let upstream = crate::control::update_upstream(&state, &sec, id, input).await?;
    Ok(Json(upstream))
}

/// Deletes an upstream.
pub async fn delete_upstream(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(sec): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let id = resource_id(&id, "upstream")?;
    crate::control::delete_upstream(&state, &sec, id)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// Creates a route.
pub async fn create_route(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(sec): Extension<SecurityContext>,
    crate::api::json::JsonBody(input): crate::api::json::JsonBody<crate::model::RouteInput>,
) -> ApiResult<(StatusCode, Json<crate::model::Route>)> {
    let route = crate::control::create_route(&state, &sec, input).await?;
    Ok((StatusCode::CREATED, Json(route)))
}

/// Lists the routes of the calling tenant.
pub async fn list_routes(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(sec): Extension<SecurityContext>,
    RawQuery(query): RawQuery,
) -> ApiResult<Json<ListEnvelope>> {
    let items = crate::control::list_routes(&state, &sec);
    let config = &state.config;
    let envelope = super::dto::list_page(items, query.as_deref(), config.list_top_default, config.list_top_max)?;
    Ok(Json(envelope))
}

/// Fetches a route.
pub async fn get_route(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(sec): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> ApiResult<Json<crate::model::Route>> {
    let id = resource_id(&id, "route")?;
    let route = crate::control::get_route(&state, &sec, id)?;
    Ok(Json(route))
}

/// Replaces a route.
pub async fn update_route(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(sec): Extension<SecurityContext>,
    Path(id): Path<String>,
    crate::api::json::JsonBody(input): crate::api::json::JsonBody<crate::model::RouteInput>,
) -> ApiResult<Json<crate::model::Route>> {
    let id = resource_id(&id, "route")?;
    let route = crate::control::update_route(&state, &sec, id, input).await?;
    Ok(Json(route))
}

/// Deletes a route.
pub async fn delete_route(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(sec): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let id = resource_id(&id, "route")?;
    crate::control::delete_route(&state, &sec, id)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

/// Creates a custom plugin definition.
pub async fn create_plugin(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(sec): Extension<SecurityContext>,
    crate::api::json::JsonBody(input): crate::api::json::JsonBody<crate::model::PluginInput>,
) -> ApiResult<(StatusCode, Json<crate::model::PluginRecord>)> {
    let record = crate::control::create_plugin(
        &state,
        &sec,
        input.name,
        input.plugin_type,
        input.config,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(record)))
}

/// Lists the custom plugins of the calling tenant.
pub async fn list_plugins(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(sec): Extension<SecurityContext>,
    RawQuery(query): RawQuery,
) -> ApiResult<Json<ListEnvelope>> {
    let items = crate::control::list_plugins(&state, &sec);
    let config = &state.config;
    let envelope = super::dto::list_page(items, query.as_deref(), config.list_top_default, config.list_top_max)?;
    Ok(Json(envelope))
}

/// Fetches a custom plugin.
pub async fn get_plugin(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(sec): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> ApiResult<Json<crate::model::PluginRecord>> {
    let id = resource_id(&id, "plugin")?;
    let record = crate::control::get_plugin(&state, &sec, id)?;
    Ok(Json(record))
}

/// Deletes a custom plugin once nothing references it.
pub async fn delete_plugin(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(sec): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let id = resource_id(&id, "plugin")?;
    crate::control::delete_plugin(&state, &sec, id)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Data plane
// ---------------------------------------------------------------------------

/// Proxies a request to the alias, without a path suffix.
pub async fn proxy_alias(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(sec): Extension<SecurityContext>,
    method: axum::http::Method,
    Path(alias): Path<String>,
    RawQuery(query): RawQuery,
    incoming: axum::extract::Request,
) -> Response {
    crate::proxy::handle(state, sec, method, alias, String::new(), query, incoming).await
}

/// Proxies a request to the alias with a path suffix.
pub async fn proxy_suffix(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(sec): Extension<SecurityContext>,
    method: axum::http::Method,
    Path((alias, path)): Path<(String, String)>,
    RawQuery(query): RawQuery,
    incoming: axum::extract::Request,
) -> Response {
    let suffix = if path.starts_with('/') {
        path
    } else {
        format!("/{path}")
    };
    crate::proxy::handle(state, sec, method, alias, suffix, query, incoming).await
}
