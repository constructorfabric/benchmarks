//! CORS enforcement (ADR-0004).
//!
//! Preflight requests (OPTIONS + `Origin` + `Access-Control-Request-Method`)
//! are answered locally with a permissive 204 that echoes the requested
//! origin/method/headers — origin and method validation is deferred to the
//! actual request. Actual requests are validated against the upstream CORS
//! config before forwarding; disallowed origins/methods are rejected with
//! 403.

use http::{HeaderMap, HeaderValue, Method, StatusCode};

use crate::domain::dto::{CorsConfig, EffectiveRoute, EffectiveUpstream};
use crate::domain::error::{DomainError, ProblemContext};

/// Whether the request is a CORS preflight (OPTIONS + Origin +
/// Access-Control-Request-Method).
#[must_use]
pub fn is_preflight(method: &Method, headers: &HeaderMap) -> bool {
    method == Method::OPTIONS
        && headers.contains_key(http::header::ORIGIN)
        && headers.contains_key("access-control-request-method")
}

/// The effective CORS config for a resolved target (route-level union, else
/// upstream).
#[must_use]
pub fn effective_cors<'a>(
    upstream: &'a EffectiveUpstream,
    route: &'a EffectiveRoute,
) -> Option<&'a CorsConfig> {
    route.cors.as_ref().or(upstream.cors.as_ref())
}

/// Build the preflight 204 response for an upstream (permissive echo of the
/// requested origin, method and headers).
#[must_use]
pub fn preflight_response(
    cors: &CorsConfig,
    origin: &str,
    requested_method: &str,
    requested_headers: Option<&str>,
) -> http::Response<axum::body::Body> {
    let mut response = http::Response::new(axum::body::Body::empty());
    *response.status_mut() = StatusCode::NO_CONTENT;
    let headers = response.headers_mut();

    headers.insert(
        "access-control-allow-origin",
        HeaderValue::from_str(origin).unwrap_or_else(|_| HeaderValue::from_static("*")),
    );
    headers.insert(
        "access-control-allow-methods",
        HeaderValue::from_str(requested_method)
            .unwrap_or_else(|_| HeaderValue::from_static("GET, POST")),
    );
    if let Some(req_headers) = requested_headers.filter(|s| !s.is_empty())
        && let Ok(value) = HeaderValue::from_str(req_headers)
    {
        headers.insert("access-control-allow-headers", value);
    }
    headers.insert("access-control-max-age", HeaderValue::from_static("86400"));
    headers.insert(
        http::header::VARY,
        HeaderValue::from_static(
            "Origin, Access-Control-Request-Method, Access-Control-Request-Headers",
        ),
    );
    if cors.allow_credentials {
        headers.insert(
            "access-control-allow-credentials",
            HeaderValue::from_static("true"),
        );
    }
    response
}

/// Validate an actual request's origin against the CORS config.
///
/// # Errors
///
/// Returns [`DomainError::CorsOriginNotAllowed`] when the origin is not
/// permitted by the CORS config.
///
/// [`DomainError::CorsOriginNotAllowed`]: crate::domain::error::DomainError::CorsOriginNotAllowed
#[allow(clippy::result_large_err)] // DomainError is a broad domain error enum; boxing would ripple through every caller
pub fn check_actual_origin(cors: &CorsConfig, origin: &str, path: &str) -> Result<(), DomainError> {
    if cors.origin_allowed(origin) {
        return Ok(());
    }
    Err(DomainError::CorsOriginNotAllowed {
        detail: format!("origin '{origin}' is not allowed"),
        context: Some(ProblemContext {
            path: Some(path.to_owned()),
            ..ProblemContext::new()
        }),
    })
}

/// Validate an actual request's method against the CORS config.
///
/// # Errors
///
/// Returns [`DomainError::CorsMethodNotAllowed`] when the method is not
/// permitted by the CORS config.
///
/// [`DomainError::CorsMethodNotAllowed`]: crate::domain::error::DomainError::CorsMethodNotAllowed
#[allow(clippy::result_large_err)] // DomainError is a broad domain error enum; boxing would ripple through every caller
pub fn check_actual_method(cors: &CorsConfig, method: &str) -> Result<(), DomainError> {
    if cors.method_allowed(method) {
        return Ok(());
    }
    Err(DomainError::CorsMethodNotAllowed {
        detail: format!("method '{method}' is not allowed by this upstream's CORS policy"),
        context: Some(ProblemContext::new()),
    })
}

/// Apply CORS response headers for an allowed actual request.
pub fn apply_actual_headers(
    response: &mut http::Response<axum::body::Body>,
    cors: &CorsConfig,
    origin: &str,
) {
    let headers = response.headers_mut();
    headers.insert(
        "access-control-allow-origin",
        HeaderValue::from_str(origin).unwrap_or_else(|_| HeaderValue::from_static("*")),
    );
    if cors.allow_credentials {
        headers.insert(
            "access-control-allow-credentials",
            HeaderValue::from_static("true"),
        );
    }
    if !cors.expose_headers.is_empty()
        && let Ok(value) = HeaderValue::from_str(&cors.expose_headers.join(", "))
    {
        headers.insert("access-control-expose-headers", value);
    }
    headers.append(http::header::VARY, HeaderValue::from_static("Origin"));
}
