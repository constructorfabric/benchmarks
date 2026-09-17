//! Built-in CORS handling (ADR-0004).
//!
//! Preflights are answered locally with a permissive 204; actual cross-origin
//! requests are validated against the resolved configuration before the
//! request reaches the upstream.

use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;

use crate::domain::error::DomainError;
use crate::domain::model::CorsConfig;

/// Header carried by every CORS-governed response.
pub const VARY_ORIGIN: &str = "Origin";
/// Lifetime of a cached preflight decision.
pub const PREFLIGHT_MAX_AGE: &str = "86400";

/// Whether the request is a CORS preflight.
#[must_use]
pub fn is_preflight(method: &str, headers: &HeaderMap) -> bool {
    method.eq_ignore_ascii_case("OPTIONS")
        && headers.contains_key(axum::http::header::ORIGIN)
        && headers.contains_key(axum::http::header::ACCESS_CONTROL_REQUEST_METHOD)
}

/// Whether the request carries an `Origin` header (a cross-origin request).
#[must_use]
pub fn is_cross_origin(headers: &HeaderMap) -> bool {
    headers.contains_key(axum::http::header::ORIGIN)
}

/// Origin presented by the client, if any.
#[must_use]
pub fn origin(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::ORIGIN)
        .and_then(|value| value.to_str().ok())
}

/// Render the permissive preflight response.
#[must_use]
pub fn preflight_response(headers: &HeaderMap) -> Response {
    let mut response = Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header("vary", VARY_ORIGIN)
        .header("access-control-max-age", PREFLIGHT_MAX_AGE);
    if let Some(origin) = origin(headers) {
        response = response.header("access-control-allow-origin", origin);
    }
    if let Some(method) = headers
        .get(axum::http::header::ACCESS_CONTROL_REQUEST_METHOD)
        .and_then(|value| value.to_str().ok())
    {
        response = response.header("access-control-allow-methods", method);
    }
    if let Some(request_headers) = headers
        .get(axum::http::header::ACCESS_CONTROL_REQUEST_HEADERS)
        .and_then(|value| value.to_str().ok())
    {
        response = response.header("access-control-allow-headers", request_headers);
    }
    response.body(axum::body::Body::empty()).unwrap_or_default()
}

/// Validate an actual cross-origin request against `config`.
///
/// # Errors
///
/// Returns `cf.oagw.cors.origin_not_allowed.v1` and
/// `cf.oagw.cors.method_not_allowed.v1` rejections.
pub fn check_request(
    config: &CorsConfig,
    headers: &HeaderMap,
    method: &str,
) -> Result<(), DomainError> {
    let Some(origin) = origin(headers) else {
        return Ok(());
    };
    if !config.origin_allowed(origin) {
        return Err(DomainError::cors_origin_not_allowed(format!(
            "origin `{origin}` is not in the allowed origins list"
        )));
    }
    if !config.method_allowed(method) {
        return Err(DomainError::cors_method_not_allowed(format!(
            "method `{method}` is not in the allowed methods list"
        )));
    }
    Ok(())
}

/// Stamp the CORS response headers on a proxied response.
pub fn apply_response_headers(config: &CorsConfig, headers: &mut HeaderMap) {
    // `Vary: Origin` is always included (ADR-0004) so a cached response is
    // never replayed to an origin the configuration does not allow.
    headers.append(
        axum::http::header::VARY,
        HeaderValue::from_static(VARY_ORIGIN),
    );
    let Some(origin) = origin(headers)
        .filter(|candidate| config.origin_allowed(candidate))
        .map(str::to_owned)
    else {
        return;
    };
    insert(headers, "access-control-allow-origin", &origin);
    insert(
        headers,
        "access-control-expose-headers",
        &config.expose_headers.join(", "),
    );
    if config.allow_credentials {
        insert(headers, "access-control-allow-credentials", "true");
    }
}

fn insert(headers: &mut HeaderMap, name: &str, value: &str) {
    if value.is_empty() {
        return;
    }
    if let Ok(name) = axum::http::HeaderName::try_from(name)
        && let Ok(value) = HeaderValue::from_str(value)
    {
        headers.insert(name, value);
    }
}

#[cfg(test)]
#[path = "cors_tests.rs"]
mod cors_tests;
