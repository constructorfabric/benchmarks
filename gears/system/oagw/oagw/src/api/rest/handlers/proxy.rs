//! Data-plane handler for `/oagw/v1/proxy/{alias}/{*path}`.

use std::sync::Arc;

use axum::Extension;
use axum::extract::{Path, Request};
use axum::response::Response;

use toolkit_security::SecurityContext;

use crate::api::rest::error::DomainError;
use crate::domain::error::ErrorKind;
use crate::proxy::cors;
use crate::proxy::data_plane::{DataPlane, ProxyRequest};

/// Peer address as text; falls back to an unspecified literal when the
/// `ConnectInfo` extension is absent (unit tests that never call
/// `into_make_service_with_connect_info`).
fn client_ip(request: &Request) -> String {
    request
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map_or_else(|| "0.0.0.0".to_owned(), |info| info.0.ip().to_string())
}

/// Normalizes the path suffix: always `/`-prefixed or empty.
fn normalize_suffix(raw: &str) -> String {
    if raw.is_empty() || raw == "/" {
        String::new()
    } else if raw.starts_with('/') {
        raw.to_owned()
    } else {
        format!("/{raw}")
    }
}

/// Answers a CORS preflight at the handler level (ADR 0004).
///
/// Preflights carry no credentials, so no tenant context exists to resolve an
/// upstream against; the response is permissive and origin/method enforcement
/// happens on the actual request after upstream resolution.
fn preflight(request: &Request) -> Option<Response> {
    let origin = cors::header_value(request.headers(), "origin")?;
    let method = cors::header_value(request.headers(), "access-control-request-method")?;
    if !cors::is_preflight(request.method(), Some(origin), Some(method)) {
        return None;
    }
    Some(cors::preflight_response(
        &crate::domain::model::CorsConfig::default(),
        origin,
        method,
        cors::header_value(request.headers(), "access-control-request-headers"),
    ))
}

/// Builds the proxy request from the axum parts.
fn build_proxy_request(
    alias: String,
    suffix: String,
    parts: http::request::Parts,
    body: axum::body::Body,
    ip: String,
    ctx: SecurityContext,
) -> ProxyRequest {
    ProxyRequest {
        alias,
        path_suffix: suffix,
        request: http::Request::from_parts(parts, body),
        client_ip: ip,
        security_context: Some(ctx),
    }
}

/// Shared pipeline body for both the suffix-less and the wildcard route.
///
/// The identity is optional at the extractor level because the route is
/// registered unauthenticated: preflights must reach the handler without
/// credentials. Anything that is not a real authenticated identity (the
/// extension absent, or the platform's anonymous placeholder) is a 401.
///
/// # Errors
///
/// Propagates the data plane's [`DomainError`]; nothing is swallowed.
async fn dispatch(
    ctx: Option<Extension<SecurityContext>>,
    plane: Arc<DataPlane>,
    (alias, raw_suffix): (String, String),
    request: Request,
) -> Result<Response, DomainError> {
    if let Some(response) = preflight(&request) {
        return Ok(response);
    }
    let ctx = ctx
        .map(|Extension(ctx)| ctx)
        .filter(|ctx| !ctx.subject_tenant_id().is_nil());
    let Some(ctx) = ctx else {
        return Err(DomainError::new(
            ErrorKind::AuthenticationFailed,
            "caller identity is required to proxy a request",
        ));
    };
    let ip = client_ip(&request);
    let suffix = normalize_suffix(&raw_suffix);
    let (parts, body) = request.into_parts();
    let proxy_request = build_proxy_request(alias, suffix, parts, body, ip, ctx.clone());
    plane.proxy(&ctx, proxy_request).await
}

/// `ALL /oagw/v1/proxy/{alias}` — no path suffix.
///
/// # Errors
///
/// Propagates the data plane's [`DomainError`].
pub async fn proxy_no_suffix(
    ctx: Option<Extension<SecurityContext>>,
    Extension(plane): Extension<Arc<DataPlane>>,
    Path(alias): Path<String>,
    request: Request,
) -> Result<Response, DomainError> {
    dispatch(ctx, plane, (alias, String::new()), request).await
}

/// `ALL /oagw/v1/proxy/{alias}/{*path}` — wildcard suffix.
///
/// # Errors
///
/// Propagates the data plane's [`DomainError`].
pub async fn proxy_with_suffix(
    ctx: Option<Extension<SecurityContext>>,
    Extension(plane): Extension<Arc<DataPlane>>,
    Path(path): Path<(String, String)>,
    request: Request,
) -> Result<Response, DomainError> {
    dispatch(ctx, plane, path, request).await
}
