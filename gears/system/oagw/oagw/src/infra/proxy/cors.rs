//! CORS helpers.
//!
//! CORS is core Data Plane logic, not a guard plugin (ADR-0004): preflight is
//! answered locally and permissively, and origin/method validation happens on
//! the actual request.

use crate::domain::error::DomainError;
use crate::domain::gts_helpers::error_id;

/// `Access-Control-Max-Age` used for preflight answers.
pub const PREFLIGHT_MAX_AGE: u64 = 86_400;

/// The `Vary` set a CORS response must carry.
#[must_use]
pub fn vary_headers() -> Vec<(String, String)> {
    vec![(
        "vary".to_owned(),
        "Origin, Access-Control-Request-Method, Access-Control-Request-Headers".to_owned(),
    )]
}

/// Just `Vary: Origin`, for responses that did not carry a preflight.
#[must_use]
pub fn vary_origin() -> Vec<(String, String)> {
    vec![("vary".to_owned(), "Origin".to_owned())]
}

/// The `403` type for a disallowed origin.
#[must_use]
pub fn origin_not_allowed_type() -> String {
    error_id("cors", "origin_not_allowed")
}

/// The `403` type for a disallowed method.
#[must_use]
pub fn method_not_allowed_type() -> String {
    error_id("cors", "method_not_allowed")
}

/// Whether the request is a CORS preflight.
#[must_use]
pub fn is_preflight(method: &http::Method, headers: &http::HeaderMap) -> bool {
    method == http::Method::OPTIONS
        && headers.get(http::header::ORIGIN).is_some()
        && headers
            .get("access-control-request-method")
            .is_some_and(|value| !value.is_empty())
}

/// Builds the permissive preflight response.
///
/// The requested origin, method and headers are echoed without consulting the
/// upstream; validation is deferred to the actual request.
#[must_use]
pub fn preflight_response(request_headers: &http::HeaderMap) -> http::Response<axum::body::Body> {
    let origin = request_headers
        .get(http::header::ORIGIN)
        .cloned()
        .unwrap_or_else(|| http::HeaderValue::from_static("*"));
    let method = request_headers
        .get("access-control-request-method")
        .cloned()
        .unwrap_or_else(|| http::HeaderValue::from_static(""));
    let request_headers_echo = request_headers
        .get("access-control-request-headers")
        .cloned()
        .unwrap_or_else(|| http::HeaderValue::from_static(""));

    let mut builder = http::Response::builder()
        .status(http::StatusCode::NO_CONTENT)
        .header(http::header::ACCESS_CONTROL_ALLOW_ORIGIN, origin)
        .header(
            http::header::ACCESS_CONTROL_ALLOW_METHODS,
            method,
        )
        .header(http::header::ACCESS_CONTROL_MAX_AGE, PREFLIGHT_MAX_AGE.to_string())
        .header("vary", "Origin, Access-Control-Request-Method, Access-Control-Request-Headers");
    if !request_headers_echo.is_empty() {
        builder = builder.header(http::header::ACCESS_CONTROL_ALLOW_HEADERS, request_headers_echo);
    }
    builder
        .body(axum::body::Body::empty())
        .unwrap_or_else(|_| http::Response::new(axum::body::Body::empty()))
}

/// Marks a `DomainError` as a CORS-origin rejection.
#[must_use]
pub fn origin_not_allowed(origin: &str) -> DomainError {
    DomainError::CorsOriginNotAllowed(format!("origin `{origin}` is not allowed"))
}

/// Builds the headers an actual cross-origin response must carry.
#[must_use]
pub fn actual_request_headers(cors: &crate::domain::dto::CorsConfig, origin: &str) -> Vec<(String, String)> {
    let mut headers = vec![(
        http::header::ACCESS_CONTROL_ALLOW_ORIGIN.to_string(),
        origin.to_owned(),
    )];
    if cors.allow_credentials {
        headers.push((
            http::header::ACCESS_CONTROL_ALLOW_CREDENTIALS.to_string(),
            "true".to_owned(),
        ));
    }
    if !cors.expose_headers.is_empty() {
        headers.push((
            http::header::ACCESS_CONTROL_EXPOSE_HEADERS.to_string(),
            cors.expose_headers.join(", "),
        ));
    }
    headers.extend(vary_origin());
    headers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preflight_is_detected() {
        let mut headers = http::HeaderMap::new();
        assert!(!is_preflight(&http::Method::OPTIONS, &headers));
        headers.insert(http::header::ORIGIN, http::HeaderValue::from_static("https://a.com"));
        assert!(!is_preflight(&http::Method::OPTIONS, &headers));
        headers.insert(
            "access-control-request-method",
            http::HeaderValue::from_static("POST"),
        );
        assert!(is_preflight(&http::Method::OPTIONS, &headers));
        assert!(!is_preflight(&http::Method::GET, &headers));
    }

    #[test]
    fn preflight_response_echoes_the_request() {
        let mut headers = http::HeaderMap::new();
        headers.insert(http::header::ORIGIN, http::HeaderValue::from_static("https://a.com"));
        headers.insert(
            "access-control-request-method",
            http::HeaderValue::from_static("POST"),
        );
        let response = preflight_response(&headers);
        assert_eq!(response.status(), http::StatusCode::NO_CONTENT);
        assert_eq!(
            response
                .headers()
                .get(http::header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|value| value.to_str().ok()),
            Some("https://a.com")
        );
    }

    #[test]
    fn error_types_use_the_cors_family() {
        assert_eq!(
            origin_not_allowed_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1"
        );
        assert_eq!(
            method_not_allowed_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1"
        );
    }
}
