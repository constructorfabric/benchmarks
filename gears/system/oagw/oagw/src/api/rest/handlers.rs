//! Axum handlers for the OAGW management and proxy planes (DESIGN §3.3).
//!
//! Management handlers are tenant-scoped through [`ControlPlaneService`]
//! and always succeed with `X-OAGW-Error-Source: gateway`. The proxy
//! handler adapts the inbound axum request to the transport-agnostic
//! [`ProxyService`] (100 MB body cap, framing validation) and forwards the
//! resulting [`ProxyResponse`] headers verbatim — the proxy pipeline owns
//! the source header on those responses.

use std::convert::Infallible;
use std::sync::Arc;

use axum::Extension;
use axum::Json;
use axum::body::to_bytes;
use axum::extract::{FromRequestParts, OriginalUri, Path, Request};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use http::header::CONTENT_TYPE;
use http::{HeaderValue, StatusCode};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::dto::{
    EntityView, PluginRequest, RouteRequest, RouteUpdateRequest, UpstreamRequest,
};
use super::error::{ApiError, gateway_source};
use crate::domain::models::Plugin;
use crate::domain::services::control_plane::ControlPlaneService;
use crate::domain::services::list::ListQuery;
use crate::infra::proxy::{
    MAX_REQUEST_BODY_BYTES, ProxyRequest, ProxyService, validate_inbound,
};

/// Path prefix the proxy route is registered under (gear-relative).
const PROXY_PREFIX: &str = "/oagw/v1/proxy";

/// Buffer-based query extractor: [`ListQuery::parse`] consumes the raw query
/// string (`OData` `$filter` / `$top` / `$skip`), never failing.
impl<S> FromRequestParts<S> for ListQuery
where
    S: Send + Sync,
{
    type Rejection = Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        Ok(ListQuery::parse(parts.uri.query().unwrap_or_default()))
    }
}

// ---------------------------------------------------------------------------
// Upstreams
// ---------------------------------------------------------------------------

/// Create an upstream.
///
/// # Errors
/// Returns an [`ApiError`] RFC 9457 problem+json response on domain or
/// data-plane failure.
pub async fn create_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Json(input): Json<UpstreamRequest>,
) -> Result<Response, ApiError> {
    let upstream = svc.create_upstream(&ctx, input.into()).await?;
    Ok(gateway_source(
        (StatusCode::CREATED, Json(EntityView::upstream(&upstream))).into_response(),
    ))
}

/// List upstreams (`$filter` / `$top` / `$skip`).
///
/// # Errors
/// Returns an [`ApiError`] RFC 9457 problem+json response on domain or
/// data-plane failure.
pub async fn list_upstreams(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    query: ListQuery,
) -> Result<Response, ApiError> {
    let items = svc.list_upstreams(&ctx, &query).await?;
    let views: Vec<EntityView> = items.iter().map(EntityView::upstream).collect();
    Ok(gateway_source(Json(views).into_response()))
}

/// Get an upstream by id.
///
/// # Errors
/// Returns an [`ApiError`] RFC 9457 problem+json response on domain or
/// data-plane failure.
pub async fn get_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Path(id): Path<Uuid>,
) -> Result<Response, ApiError> {
    let upstream = svc.get_upstream(&ctx, id).await?;
    Ok(gateway_source(
        (StatusCode::OK, Json(EntityView::upstream(&upstream))).into_response(),
    ))
}

/// Replace an upstream in full (`PUT`; alias immutable).
///
/// # Errors
/// Returns an [`ApiError`] RFC 9457 problem+json response on domain or
/// data-plane failure.
pub async fn update_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Path(id): Path<Uuid>,
    Json(input): Json<UpstreamRequest>,
) -> Result<Response, ApiError> {
    let upstream = svc.update_upstream(&ctx, id, input.into()).await?;
    Ok(gateway_source(
        (StatusCode::OK, Json(EntityView::upstream(&upstream))).into_response(),
    ))
}

/// Delete an upstream (409 while routes reference it).
///
/// # Errors
/// Returns an [`ApiError`] RFC 9457 problem+json response on domain or
/// data-plane failure.
pub async fn delete_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Path(id): Path<Uuid>,
) -> Result<Response, ApiError> {
    svc.delete_upstream(&ctx, id).await?;
    Ok(gateway_source(StatusCode::NO_CONTENT.into_response()))
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// Create a route.
///
/// # Errors
/// Returns an [`ApiError`] RFC 9457 problem+json response on domain or
/// data-plane failure.
pub async fn create_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Json(input): Json<RouteRequest>,
) -> Result<Response, ApiError> {
    let route = svc.create_route(&ctx, input.into()).await?;
    Ok(gateway_source(
        (StatusCode::CREATED, Json(EntityView::route(&route))).into_response(),
    ))
}

/// List routes (`$filter` / `$top` / `$skip`).
///
/// # Errors
/// Returns an [`ApiError`] RFC 9457 problem+json response on domain or
/// data-plane failure.
pub async fn list_routes(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    query: ListQuery,
) -> Result<Response, ApiError> {
    let items = svc.list_routes(&ctx, &query).await?;
    let views: Vec<EntityView> = items.iter().map(EntityView::route).collect();
    Ok(gateway_source(Json(views).into_response()))
}

/// Get a route by id.
///
/// # Errors
/// Returns an [`ApiError`] RFC 9457 problem+json response on domain or
/// data-plane failure.
pub async fn get_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Path(id): Path<Uuid>,
) -> Result<Response, ApiError> {
    let route = svc.get_route(&ctx, id).await?;
    Ok(gateway_source(
        (StatusCode::OK, Json(EntityView::route(&route))).into_response(),
    ))
}

/// Replace a route in full (`PUT`; `upstream_id` immutable).
///
/// # Errors
/// Returns an [`ApiError`] RFC 9457 problem+json response on domain or
/// data-plane failure.
pub async fn update_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Path(id): Path<Uuid>,
    Json(input): Json<RouteUpdateRequest>,
) -> Result<Response, ApiError> {
    // The update DTO omits `upstream_id`; keep the existing binding.
    let existing = svc.get_route(&ctx, id).await?;
    let route = svc
        .update_route(&ctx, id, input.into_input(existing.upstream_id))
        .await?;
    Ok(gateway_source(
        (StatusCode::OK, Json(EntityView::route(&route))).into_response(),
    ))
}

/// Delete a route.
///
/// # Errors
/// Returns an [`ApiError`] RFC 9457 problem+json response on domain or
/// data-plane failure.
pub async fn delete_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Path(id): Path<Uuid>,
) -> Result<Response, ApiError> {
    svc.delete_route(&ctx, id).await?;
    Ok(gateway_source(StatusCode::NO_CONTENT.into_response()))
}

// ---------------------------------------------------------------------------
// Custom plugins
// ---------------------------------------------------------------------------

/// Create a custom plugin (immutable after creation).
///
/// # Errors
/// Returns an [`ApiError`] RFC 9457 problem+json response on domain or
/// data-plane failure.
pub async fn create_plugin(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Json(input): Json<PluginRequest>,
) -> Result<Response, ApiError> {
    let plugin = svc.create_plugin(&ctx, input.into()).await?;
    Ok(gateway_source(
        (StatusCode::CREATED, Json(EntityView::plugin(&plugin))).into_response(),
    ))
}

/// List custom plugins (`$filter` / `$top` / `$skip`).
///
/// # Errors
/// Returns an [`ApiError`] RFC 9457 problem+json response on domain or
/// data-plane failure.
pub async fn list_plugins(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    query: ListQuery,
) -> Result<Response, ApiError> {
    let items = svc.list_plugins(&ctx, &query).await?;
    let views: Vec<EntityView> = items.iter().map(EntityView::plugin).collect();
    Ok(gateway_source(Json(views).into_response()))
}

/// Get a custom plugin by id.
///
/// # Errors
/// Returns an [`ApiError`] RFC 9457 problem+json response on domain or
/// data-plane failure.
pub async fn get_plugin(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Path(id): Path<Uuid>,
) -> Result<Response, ApiError> {
    let plugin = svc.get_plugin(&ctx, id).await?;
    Ok(gateway_source(
        (StatusCode::OK, Json(EntityView::plugin(&plugin))).into_response(),
    ))
}

/// Delete a custom plugin (409 while referenced by upstreams/routes).
///
/// # Errors
/// Returns an [`ApiError`] RFC 9457 problem+json response on domain or
/// data-plane failure.
pub async fn delete_plugin(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Path(id): Path<Uuid>,
) -> Result<Response, ApiError> {
    svc.delete_plugin(&ctx, id).await?;
    Ok(gateway_source(StatusCode::NO_CONTENT.into_response()))
}

/// Retrieve the plugin source (`GET .../plugins/{id}/source`).
///
/// Custom plugins here are parameterized builtin instances (no sandboxed
/// Starlark body exists), so the "source" is the instantiation statement.
///
/// # Errors
/// Returns an [`ApiError`] RFC 9457 problem+json response on domain or
/// data-plane failure.
pub async fn get_plugin_source(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Path(id): Path<Uuid>,
) -> Result<Response, ApiError> {
    let plugin = svc.get_plugin(&ctx, id).await?;
    let mut response = (
        StatusCode::OK,
        [(CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"))],
        starlark_source(&plugin),
    )
        .into_response();
    response = gateway_source(response);
    Ok(response)
}

/// A Starlark-shaped representation of a parameterized builtin plugin.
fn starlark_source(plugin: &Plugin) -> String {
    let config = serde_json::to_string_pretty(&plugin.config)
        .unwrap_or_else(|_| "{}".to_owned());
    format!(
        "# OAGW custom plugin '{name}' — parameterized builtin (no sandboxed Starlark body).\n\
        plugin(\n    type = \"{builtin_type}\",\n    name = \"{name}\",\n    config = {config},\n)\n",
        name = plugin.name,
        builtin_type = plugin.builtin_type,
    )
}

// ---------------------------------------------------------------------------
// Proxy (data plane)
// ---------------------------------------------------------------------------

/// Proxy handler: adapt the inbound request to the data plane and forward
/// the (already source-tagged) response.
///
/// # Errors
/// Returns an [`ApiError`] RFC 9457 problem+json response on domain or
/// data-plane failure.
pub async fn proxy(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ProxyService>>,
    Path((alias, _suffix)): Path<(String, String)>,
    uri: OriginalUri,
    request: Request,
) -> Result<Response, ApiError> {
    // Reconstruct the forwarded path after the alias from the raw URI so
    // percent-encoding is preserved ("percent-encoded as received").
    let prefix = format!("{PROXY_PREFIX}/{alias}");
    let suffix = uri.path().strip_prefix(&prefix).unwrap_or_default();
    let path = if suffix.is_empty() {
        "/".to_owned()
    } else {
        suffix.to_owned()
    };
    let query = uri.query().map(str::to_owned);

    // Reject before buffering when the declared Content-Length already
    // exceeds the hard cap (DESIGN: "prevent resource exhaustion; reject
    // before buffering"). Chunked bodies without a length are bounded by
    // the buffering limit below.
    if let Some(value) = request.headers().get(http::header::CONTENT_LENGTH)
        && let Ok(raw) = value.to_str()
        && let Ok(declared) = raw.trim().parse::<u64>()
        && declared > MAX_REQUEST_BODY_BYTES as u64
    {
        return Err(ApiError::payload_too_large(&alias, &path));
    }
    let method = request.method().clone();
    let headers = request.headers().clone();
    let body = to_bytes(request.into_body(), MAX_REQUEST_BODY_BYTES + 1)
        .await
        .map_err(ApiError::body_read)?;
    if body.len() > MAX_REQUEST_BODY_BYTES {
        return Err(ApiError::payload_too_large(&alias, &path));
    }
    validate_inbound(&headers, body.len())?;

    let proxy_request = ProxyRequest {
        method,
        path,
        query,
        headers,
        body,
    };
    let response = svc.handle(&ctx, proxy_request, &alias).await?;
    Ok((response.status, response.headers, response.body).into_response())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn starlark_source_describes_builtin_instantiation() {
        let plugin = Plugin {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            kind: crate::domain::models::PluginKind::Guard,
            builtin_type: "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"
                .to_owned(),
            name: "legacy-guard".to_owned(),
            config: serde_json::json!({ "headers": ["X-Api-Key"] }),
        };
        let source = starlark_source(&plugin);
        assert!(source.contains("plugin("));
        assert!(source.contains("type = \"gts.cf.core.oagw.guard_plugin.v1~"));
        assert!(source.contains("name = \"legacy-guard\""));
        assert!(source.contains("\"X-Api-Key\""));
    }

    #[test]
    fn proxy_path_derivation_matches_wire_prefixes() {
        let uri = http::Uri::from_static("/oagw/v1/proxy/api.openai.com/v1/chat%2Fmeta");
        let prefix = format!("{PROXY_PREFIX}/api.openai.com");
        let suffix = uri.path().strip_prefix(&prefix).unwrap_or_default();
        assert_eq!(suffix, "/v1/chat%2Fmeta");
        // Bare alias (no suffix) forwards to the upstream root.
        let bare = http::Uri::from_static("/oagw/v1/proxy/api.openai.com");
        let suffix = bare.path().strip_prefix(&prefix).unwrap_or_default();
        assert!(suffix.is_empty());
        // Trailing slash collapses to the root as well.
        let slash = http::Uri::from_static("/oagw/v1/proxy/api.openai.com/");
        let suffix = slash.path().strip_prefix(&prefix).unwrap_or_default();
        assert!(suffix.is_empty() || suffix == "/");
    }
}
