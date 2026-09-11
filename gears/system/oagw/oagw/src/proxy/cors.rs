//! CORS handling on the proxy path.
//!
//! See ADR-0004: preflight is answered permissively at the handler level; the
//! actual request is validated against the effective origin allowlist.

use http::{HeaderMap, Method, header};

use crate::domain::error::OagwError;

/// Whether the request is a CORS preflight.
#[must_use]
pub fn is_preflight(method: &Method, headers: &HeaderMap) -> bool {
    method == Method::OPTIONS
        && headers.contains_key(header::ORIGIN)
        && headers.contains_key(header::ACCESS_CONTROL_REQUEST_METHOD)
}

/// Headers attached to a permissive preflight response.
#[must_use]
pub fn preflight_response(request_headers: &HeaderMap) -> axum::response::Response {
    let origin = request_headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("*");
    let method = request_headers
        .get(header::ACCESS_CONTROL_REQUEST_METHOD)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("*");
    let requested = request_headers
        .get(header::ACCESS_CONTROL_REQUEST_HEADERS)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let mut response = axum::http::Response::builder()
        .status(http::StatusCode::NO_CONTENT)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin)
        .header(header::ACCESS_CONTROL_ALLOW_METHODS, method)
        .header(header::ACCESS_CONTROL_MAX_AGE, "86400")
        .header(
            header::VARY,
            "Origin, Access-Control-Request-Method, Access-Control-Request-Headers",
        );
    if !requested.is_empty()
        && let Ok(value) = http::HeaderValue::from_str(requested)
    {
        response = response.header(header::ACCESS_CONTROL_ALLOW_HEADERS, value);
    }
    response
        .body(axum::body::Body::empty())
        .unwrap_or_else(|_| axum::http::Response::new(axum::body::Body::empty()))
}

/// Validates an actual request's origin against the effective CORS config.
///
/// # Errors
///
/// 403 `cors.origin_not_allowed.v1` or `cors.method_not_allowed.v1`.
pub fn validate_request(
    cors: &crate::domain::model::Cors,
    method: &Method,
    headers: &HeaderMap,
) -> Result<(), OagwError> {
    if !cors.enabled {
        return Ok(());
    }
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return Ok(());
    };

    let origins = cors.effective_origins();
    let origin_allowed = origins.iter().any(|o| o == "*" || o == origin);
    if !origin_allowed {
        return Err(OagwError::cors_origin_not_allowed(format!(
            "origin {origin:?} is not in allowed_origins"
        ))
        .with_extension("origin", origin));
    }

    let methods = cors.effective_methods();
    if !methods
        .iter()
        .any(|m| m.eq_ignore_ascii_case(method.as_str()))
    {
        return Err(OagwError::cors_method_not_allowed(format!(
            "method {method} is not in allowed_methods"
        ))
        .with_extension("method", method.as_str()));
    }
    Ok(())
}

/// CORS response headers for a successful proxied response.
pub fn response_headers(
    cors: Option<&crate::domain::model::Cors>,
    request_headers: &HeaderMap,
    response_headers: &mut HeaderMap,
) {
    let Some(cors) = cors else {
        return;
    };
    if !cors.enabled {
        return;
    }
    let Some(origin) = request_headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
    else {
        return;
    };
    let origins = cors.effective_origins();
    if origins.iter().any(|o| o == "*" || o == origin) {
        if let Ok(value) = http::HeaderValue::from_str(if origins.iter().any(|o| o == "*") {
            "*"
        } else {
            origin
        }) {
            response_headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, value);
        }
        if cors.allow_credentials {
            response_headers.insert(
                header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
                http::HeaderValue::from_static("true"),
            );
        }
        if !cors.expose_headers.is_empty() {
            let value = cors.expose_headers.join(", ");
            if let Ok(value) = http::HeaderValue::from_str(&value) {
                response_headers.insert(header::ACCESS_CONTROL_EXPOSE_HEADERS, value);
            }
        }
        response_headers.insert(header::VARY, http::HeaderValue::from_static("Origin"));
    }
}
