//! Axum handlers.
//!
//! Path-based dispatch (ADR-0001): `/upstreams`, `/routes` and `/plugins` go
//! to the Control Plane, `/proxy` to the Data Plane.

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Extension, Path, Request};
use axum::response::{IntoResponse, Response};
use http::{StatusCode, Uri, header};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::error::problem_response;
use crate::domain::dto::{PluginWriteInput, RouteWriteInput, UpstreamWriteInput};
use crate::domain::error::{OagwError, OagwResult};
use crate::domain::gts_helpers;
use crate::domain::services::DataPlaneService;
use crate::domain::services::management::{
    ControlPlaneService, plugin_json, route_json, upstream_json,
};
use crate::domain::services::odata;
use crate::infra::cors;
use crate::infra::ratelimit::RateLimiterRegistry;
use crate::util::ERROR_SOURCE_HEADER;

/// Shared handler state.
pub struct OagwState {
    pub control_plane: Arc<ControlPlaneService>,
    pub data_plane: Arc<dyn DataPlaneService>,
    /// Data-Plane-owned rate limiters, swept when their resource is deleted
    /// (ADR-0003: the `{resource_type}:{resource_id}` key prefix exists for
    /// exactly this).
    pub limiter: Arc<RateLimiterRegistry>,
}

type State = Extension<Arc<OagwState>>;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_body<T: DeserializeOwned>(body: &Bytes) -> OagwResult<T> {
    if body.is_empty() {
        return Err(OagwError::validation("a JSON request body is required"));
    }
    serde_json::from_slice(body)
        .map_err(|err| OagwError::validation(format!("invalid request body: {err}")))
}

fn json_response(status: StatusCode, payload: &Value) -> Response {
    let bytes = serde_json::to_vec(payload).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(bytes))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn render(result: OagwResult<Response>, uri: &Uri) -> Response {
    match result {
        Ok(response) => response,
        Err(err) => problem_response(&err, Some(uri.path())),
    }
}

fn parse_path_id(raw: &str, base_type: &str) -> OagwResult<Uuid> {
    gts_helpers::parse_resource_id(raw, base_type).ok_or_else(|| {
        OagwError::validation(format!(
            "'{raw}' is not a valid identifier (expected a UUID or '{base_type}{{uuid}}')"
        ))
    })
}

/// Envelope for list endpoints: `items` plus the applied page size, matching
/// the platform's paged-collection shape.
fn list_response(items: Vec<Value>, top: usize) -> Response {
    let total = items.len();
    json_response(
        StatusCode::OK,
        &json!({
            "items": items,
            "total": total,
            "page_info": { "next_cursor": Value::Null, "prev_cursor": Value::Null, "limit": top },
        }),
    )
}

// ---------------------------------------------------------------------------
// Upstreams
// ---------------------------------------------------------------------------

pub async fn create_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): State,
    uri: Uri,
    body: Bytes,
) -> Response {
    let result = async {
        let input: UpstreamWriteInput = parse_body(&body)?;
        let created = state.control_plane.create_upstream(&ctx, input).await?;
        let mut response = json_response(StatusCode::CREATED, &upstream_json(&created));
        if let Ok(location) = http::HeaderValue::from_str(&format!(
            "{}/{}",
            uri.path().trim_end_matches('/'),
            created.id
        )) {
            response.headers_mut().insert(header::LOCATION, location);
        }
        Ok(response)
    }
    .await;
    render(result, &uri)
}

pub async fn list_upstreams(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): State,
    uri: Uri,
) -> Response {
    let result = async {
        let query = odata::parse_list_query(uri.query())?;
        let rows = state
            .control_plane
            .list_upstreams(&ctx)
            .await?
            .iter()
            .map(upstream_json)
            .collect();
        let top = query.top.unwrap_or(odata::DEFAULT_TOP);
        Ok(list_response(odata::apply(&query, rows)?, top))
    }
    .await;
    render(result, &uri)
}

pub async fn get_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): State,
    Path(id): Path<String>,
    uri: Uri,
) -> Response {
    let result = async {
        let id = parse_path_id(&id, gts_helpers::UPSTREAM_TYPE)?;
        let found = state.control_plane.get_upstream(&ctx, id).await?;
        Ok(json_response(StatusCode::OK, &upstream_json(&found)))
    }
    .await;
    render(result, &uri)
}

pub async fn replace_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): State,
    Path(id): Path<String>,
    uri: Uri,
    body: Bytes,
) -> Response {
    let result = async {
        let id = parse_path_id(&id, gts_helpers::UPSTREAM_TYPE)?;
        let input: UpstreamWriteInput = parse_body(&body)?;
        let updated = state.control_plane.replace_upstream(&ctx, id, input).await?;
        Ok(json_response(StatusCode::OK, &upstream_json(&updated)))
    }
    .await;
    render(result, &uri)
}

pub async fn delete_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): State,
    Path(id): Path<String>,
    uri: Uri,
) -> Response {
    let result = async {
        let id = parse_path_id(&id, gts_helpers::UPSTREAM_TYPE)?;
        state.control_plane.delete_upstream(&ctx, id).await?;
        state
            .limiter
            .purge_prefix(&format!("oagw:ratelimit:upstream:{id}:"));
        Ok(StatusCode::NO_CONTENT.into_response())
    }
    .await;
    render(result, &uri)
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

pub async fn create_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): State,
    uri: Uri,
    body: Bytes,
) -> Response {
    let result = async {
        let input: RouteWriteInput = parse_body(&body)?;
        let created = state.control_plane.create_route(&ctx, input).await?;
        let mut response = json_response(StatusCode::CREATED, &route_json(&created));
        if let Ok(location) = http::HeaderValue::from_str(&format!(
            "{}/{}",
            uri.path().trim_end_matches('/'),
            created.id
        )) {
            response.headers_mut().insert(header::LOCATION, location);
        }
        Ok(response)
    }
    .await;
    render(result, &uri)
}

pub async fn list_routes(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): State,
    uri: Uri,
) -> Response {
    let result = async {
        let query = odata::parse_list_query(uri.query())?;
        let rows = state
            .control_plane
            .list_routes(&ctx)
            .await?
            .iter()
            .map(route_json)
            .collect();
        let top = query.top.unwrap_or(odata::DEFAULT_TOP);
        Ok(list_response(odata::apply(&query, rows)?, top))
    }
    .await;
    render(result, &uri)
}

pub async fn get_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): State,
    Path(id): Path<String>,
    uri: Uri,
) -> Response {
    let result = async {
        let id = parse_path_id(&id, gts_helpers::ROUTE_TYPE)?;
        let found = state.control_plane.get_route(&ctx, id).await?;
        Ok(json_response(StatusCode::OK, &route_json(&found)))
    }
    .await;
    render(result, &uri)
}

pub async fn replace_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): State,
    Path(id): Path<String>,
    uri: Uri,
    body: Bytes,
) -> Response {
    let result = async {
        let id = parse_path_id(&id, gts_helpers::ROUTE_TYPE)?;
        let input: RouteWriteInput = parse_body(&body)?;
        let updated = state.control_plane.replace_route(&ctx, id, input).await?;
        Ok(json_response(StatusCode::OK, &route_json(&updated)))
    }
    .await;
    render(result, &uri)
}

pub async fn delete_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): State,
    Path(id): Path<String>,
    uri: Uri,
) -> Response {
    let result = async {
        let id = parse_path_id(&id, gts_helpers::ROUTE_TYPE)?;
        state.control_plane.delete_route(&ctx, id).await?;
        state
            .limiter
            .purge_prefix(&format!("oagw:ratelimit:route:{id}:"));
        Ok(StatusCode::NO_CONTENT.into_response())
    }
    .await;
    render(result, &uri)
}

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

pub async fn create_plugin(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): State,
    uri: Uri,
    body: Bytes,
) -> Response {
    let result = async {
        let input: PluginWriteInput = parse_body(&body)?;
        let created = state.control_plane.create_plugin(&ctx, input).await?;
        let mut response = json_response(StatusCode::CREATED, &plugin_json(&created));
        if let Ok(location) = http::HeaderValue::from_str(&format!(
            "{}/{}",
            uri.path().trim_end_matches('/'),
            created.gts_id()
        )) {
            response.headers_mut().insert(header::LOCATION, location);
        }
        Ok(response)
    }
    .await;
    render(result, &uri)
}

pub async fn list_plugins(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): State,
    uri: Uri,
) -> Response {
    let result = async {
        let query = odata::parse_list_query(uri.query())?;
        let rows = state
            .control_plane
            .list_plugins(&ctx)
            .await?
            .iter()
            .map(plugin_json)
            .collect();
        let top = query.top.unwrap_or(odata::DEFAULT_TOP);
        Ok(list_response(odata::apply(&query, rows)?, top))
    }
    .await;
    render(result, &uri)
}

/// Accept any of the three plugin base types (or a bare UUID) in the path.
fn parse_plugin_id(raw: &str) -> OagwResult<Uuid> {
    for base in [
        gts_helpers::AUTH_PLUGIN_TYPE,
        gts_helpers::GUARD_PLUGIN_TYPE,
        gts_helpers::TRANSFORM_PLUGIN_TYPE,
    ] {
        if let Some(id) = gts_helpers::parse_resource_id(raw, base) {
            return Ok(id);
        }
    }
    Err(OagwError::validation(format!(
        "'{raw}' is not a valid plugin identifier (expected a UUID or \
         'gts.cf.core.oagw.{{type}}_plugin.v1~{{uuid}}')"
    )))
}

pub async fn get_plugin(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): State,
    Path(id): Path<String>,
    uri: Uri,
) -> Response {
    let result = async {
        let id = parse_plugin_id(&id)?;
        let found = state.control_plane.get_plugin(&ctx, id).await?;
        Ok(json_response(StatusCode::OK, &plugin_json(&found)))
    }
    .await;
    render(result, &uri)
}

pub async fn get_plugin_source(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): State,
    Path(id): Path<String>,
    uri: Uri,
) -> Response {
    let result = async {
        let id = parse_plugin_id(&id)?;
        let found = state.control_plane.get_plugin(&ctx, id).await?;
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .body(Body::from(found.source_code))
            .map_err(|err| OagwError::internal(format!("could not build response: {err}")))
    }
    .await;
    render(result, &uri)
}

pub async fn delete_plugin(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): State,
    Path(id): Path<String>,
    uri: Uri,
) -> Response {
    let result = async {
        let id = parse_plugin_id(&id)?;
        state.control_plane.delete_plugin(&ctx, id).await?;
        Ok(StatusCode::NO_CONTENT.into_response())
    }
    .await;
    render(result, &uri)
}

// ---------------------------------------------------------------------------
// Proxy
// ---------------------------------------------------------------------------

pub async fn proxy_root(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): State,
    Path(alias): Path<String>,
    request: Request,
) -> Response {
    proxy(&ctx, &state, &alias, String::new(), request).await
}

pub async fn proxy_with_path(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): State,
    Path((alias, rest)): Path<(String, String)>,
    request: Request,
) -> Response {
    let suffix = if rest.is_empty() {
        String::new()
    } else {
        format!("/{}", rest.trim_start_matches('/'))
    };
    proxy(&ctx, &state, &alias, suffix, request).await
}

async fn proxy(
    ctx: &SecurityContext,
    state: &Arc<OagwState>,
    alias: &str,
    path_suffix: String,
    request: Request,
) -> Response {
    let uri = request.uri().clone();

    // CORS preflight is answered before any upstream resolution: a browser
    // sends no credentials on preflight, so there is no tenant to resolve one
    // with (ADR-0004).
    if cors::is_preflight(request.method(), request.headers()) {
        let mut response = Response::builder()
            .status(cors::preflight_status())
            .body(Body::empty())
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
        let headers = cors::preflight_headers(request.headers());
        cors::merge_into(response.headers_mut(), headers);
        response.headers_mut().insert(
            http::HeaderName::from_static(ERROR_SOURCE_HEADER),
            http::HeaderValue::from_static("gateway"),
        );
        return response;
    }

    match state
        .data_plane
        .execute_proxy(ctx, alias, &path_suffix, request)
        .await
    {
        Ok(response) => response,
        Err(err) => problem_response(&err, Some(uri.path())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_ids_parse_from_every_base_type() {
        let id = Uuid::new_v4();
        for base in [
            gts_helpers::AUTH_PLUGIN_TYPE,
            gts_helpers::GUARD_PLUGIN_TYPE,
            gts_helpers::TRANSFORM_PLUGIN_TYPE,
        ] {
            let raw = gts_helpers::anonymous_id(base, id);
            assert_eq!(parse_plugin_id(&raw).expect("parse"), id);
        }
        assert_eq!(parse_plugin_id(&id.to_string()).expect("bare uuid"), id);
        assert!(parse_plugin_id("not-an-id").is_err());
    }

    #[test]
    fn resource_ids_reject_a_foreign_base_type() {
        let id = Uuid::new_v4();
        let route_id = gts_helpers::anonymous_id(gts_helpers::ROUTE_TYPE, id);
        assert!(parse_path_id(&route_id, gts_helpers::UPSTREAM_TYPE).is_err());
        assert!(parse_path_id(&route_id, gts_helpers::ROUTE_TYPE).is_ok());
    }

    #[test]
    fn an_empty_body_is_a_validation_error() {
        let err = parse_body::<UpstreamWriteInput>(&Bytes::new()).expect_err("empty");
        assert_eq!(err.status(), 400);
    }
}
