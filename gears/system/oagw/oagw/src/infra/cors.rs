//! CORS handling per ADR 0004.
//!
//! Preflight requests (`OPTIONS` + `Origin` + `Access-Control-Request-Method`)
//! are answered at the handler level with a permissive 204 that echoes the
//! requested origin / method / headers — no upstream resolution, no tenant
//! context. Actual requests are validated against the resolved upstream's
//! CORS config before forwarding.

use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::domain::error::OagwError;
use crate::domain::model::CorsConfig;

/// Whether `method` + request headers mark a CORS preflight request.
#[must_use]
pub fn is_preflight(method: &Method, headers: &HeaderMap) -> bool {
    method == Method::OPTIONS
        && headers.contains_key("origin")
        && headers.contains_key("access-control-request-method")
}

/// Build the permissive 204 preflight response per ADR 0004:
/// echoes request origin, method and headers, `Max-Age: 86400`, and
/// `Vary` covering the preflight inputs.
#[must_use]
pub fn preflight_response(headers: &HeaderMap) -> Response {
    let origin = headers
        .get("origin")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("*");
    let method = headers
        .get("access-control-request-method")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("GET");
    let request_headers = headers
        .get("access-control-request-headers")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let response = (StatusCode::NO_CONTENT, ()).into_response();
    let mut response = response;
    let headers = response.headers_mut();
    let origin_value = HeaderValue::from_str(origin).unwrap_or_else(|_| HeaderValue::from_static("*"));
    headers.insert("access-control-allow-origin", origin_value);
    if let Ok(method_value) = HeaderValue::from_str(method) {
        headers.insert("access-control-allow-methods", method_value);
    }
    if !request_headers.is_empty() {
        if let Ok(value) = HeaderValue::from_str(request_headers) {
            headers.insert("access-control-allow-headers", value);
        }
    }
    headers.insert("access-control-max-age", HeaderValue::from_static("86400"));
    headers.insert(
        "vary",
        HeaderValue::from_static("Origin, Access-Control-Request-Method, Access-Control-Request-Headers"),
    );
    response
}

/// Validate an actual (non-preflight) cross-origin request against the
/// upstream's CORS configuration. Requests without an `Origin` header
/// are not cross-origin — they bypass CORS enforcement.
pub fn validate_actual_request(
    cors: Option<&CorsConfig>,
    headers: &HeaderMap,
    method: &Method,
) -> Result<(), OagwError> {
    let Some(origin) = headers.get("origin").and_then(|v| v.to_str().ok()) else {
        return Ok(());
    };
    let Some(cors) = cors else {
        // No CORS config → nothing to enforce (origin passes through
        // as a plain header).
        return Ok(());
    };
    if !cors.enabled {
        return Ok(());
    }
    if !cors.origin_allowed(origin) {
        return Err(OagwError::cors_origin_not_allowed(format!(
            "Origin '{origin}' not in allowed origins list"
        )));
    }
    if !cors.method_allowed(method.as_str()) {
        return Err(OagwError::cors_method_not_allowed(format!(
            "Method '{}' not in allowed methods list",
            method.as_str()
        )));
    }
    Ok(())
}

/// Attach CORS response headers to an actual-request response.
pub fn apply_response_headers(cors: Option<&CorsConfig>, headers: &mut HeaderMap, origin: Option<&str>) {
    let Some(cors) = cors else { return };
    if !cors.enabled {
        return;
    }
    let Some(origin) = origin else { return };
    // Echo the exact origin (or `*` for the wildcard config).
    if cors.allowed_origins.iter().any(|o| o == "*") {
        headers.insert("access-control-allow-origin", HeaderValue::from_static("*"));
    } else {
        if let Ok(value) = HeaderValue::from_str(origin) {
            headers.insert("access-control-allow-origin", value);
        }
    }
    if !cors.expose_headers.is_empty() {
        let joined = cors.expose_headers.join(", ");
        if let Ok(value) = HeaderValue::from_str(&joined) {
            headers.insert("access-control-expose-headers", value);
        }
    }
    if cors.allow_credentials {
        headers.insert(
            "access-control-allow-credentials",
            HeaderValue::from_static("true"),
        );
    }
    append_vary(headers, "Origin");
}

fn append_vary(headers: &mut HeaderMap, value: &str) {
    let existing = headers
        .get("vary")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .unwrap_or_default();
    let joined = if existing.is_empty() {
        value.to_owned()
    } else if existing.split(',').any(|v| v.trim().eq_ignore_ascii_case(value)) {
        existing
    } else {
        format!("{existing}, {value}")
    };
    if let Ok(v) = HeaderValue::from_str(&joined) {
        headers.insert("vary", v);
    }
}
