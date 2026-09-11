//! CORS preflight detection and the permissive `204` response
//! (`cpt-cf-oagw-flow-cors-preflight`, `cpt-cf-oagw-dod-cors-preflight`,
//! `cpt-cf-oagw-adr-cors`).

use axum::http::header::{
    ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN,
    ACCESS_CONTROL_MAX_AGE, ACCESS_CONTROL_REQUEST_HEADERS, ACCESS_CONTROL_REQUEST_METHOD, ORIGIN,
    VARY,
};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};

/// The advertised preflight cache lifetime, in seconds
/// (`cpt-cf-oagw-dod-cors-preflight`).
const MAX_AGE_SECONDS: &str = "86400";

/// `true` when `method` is `OPTIONS` and `headers` carry both `Origin` and
/// `Access-Control-Request-Method` — the two markers WHATWG Fetch requires
/// for a genuine preflight, distinguishing it from an ordinary `OPTIONS`
/// proxy request (`cpt-cf-oagw-dod-cors-preflight`).
#[must_use]
pub fn is_preflight_request(method: &Method, headers: &HeaderMap) -> bool {
    *method == Method::OPTIONS
        && headers.contains_key(ORIGIN)
        && headers.contains_key(ACCESS_CONTROL_REQUEST_METHOD)
}

/// Builds the permissive `204 No Content` preflight response, echoing the
/// requested origin, method, and headers, before any upstream resolution
/// (`cpt-cf-oagw-dod-cors-preflight`, `cpt-cf-oagw-flow-cors-preflight`).
// @cpt-begin:cpt-cf-oagw-dod-cors-preflight:p1:inst-cors-preflight-fn-01
#[must_use]
pub fn build_preflight_response(headers: &HeaderMap) -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    let out = response.headers_mut();

    if let Some(origin) = headers.get(ORIGIN) {
        out.insert(ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
    }
    if let Some(requested_method) = headers.get(ACCESS_CONTROL_REQUEST_METHOD) {
        out.insert(ACCESS_CONTROL_ALLOW_METHODS, requested_method.clone());
    }
    if let Some(requested_headers) = headers.get(ACCESS_CONTROL_REQUEST_HEADERS) {
        out.insert(ACCESS_CONTROL_ALLOW_HEADERS, requested_headers.clone());
    }
    out.insert(
        ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static(MAX_AGE_SECONDS),
    );
    out.insert(
        VARY,
        HeaderValue::from_static(
            "Origin, Access-Control-Request-Method, Access-Control-Request-Headers",
        ),
    );
    response
}
// @cpt-end:cpt-cf-oagw-dod-cors-preflight:p1:inst-cors-preflight-fn-01

#[cfg(test)]
mod tests {
    use super::{build_preflight_response, is_preflight_request};
    use axum::http::{HeaderMap, HeaderValue, Method};

    fn preflight_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "origin",
            HeaderValue::from_static("https://app.example.com"),
        );
        headers.insert(
            "access-control-request-method",
            HeaderValue::from_static("POST"),
        );
        headers.insert(
            "access-control-request-headers",
            HeaderValue::from_static("Content-Type, Authorization"),
        );
        headers
    }

    // @cpt-begin:cpt-cf-oagw-dod-cors-preflight:p1:inst-cors-preflight-detect-test-01
    #[test]
    fn options_with_origin_and_request_method_is_a_preflight() {
        assert!(is_preflight_request(&Method::OPTIONS, &preflight_headers()));
    }

    #[test]
    fn options_without_request_method_is_not_a_preflight() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "origin",
            HeaderValue::from_static("https://app.example.com"),
        );
        assert!(!is_preflight_request(&Method::OPTIONS, &headers));
    }

    #[test]
    fn a_non_options_request_is_never_a_preflight() {
        assert!(!is_preflight_request(&Method::POST, &preflight_headers()));
    }
    // @cpt-end:cpt-cf-oagw-dod-cors-preflight:p1:inst-cors-preflight-detect-test-01

    // @cpt-begin:cpt-cf-oagw-dod-cors-preflight:p1:inst-cors-preflight-response-test-01
    #[test]
    fn the_preflight_response_echoes_origin_method_and_headers() {
        let response = build_preflight_response(&preflight_headers());
        assert_eq!(response.status(), axum::http::StatusCode::NO_CONTENT);
        let headers = response.headers();
        assert_eq!(
            headers
                .get("access-control-allow-origin")
                .and_then(|v| v.to_str().ok()),
            Some("https://app.example.com")
        );
        assert_eq!(
            headers
                .get("access-control-allow-methods")
                .and_then(|v| v.to_str().ok()),
            Some("POST")
        );
        assert_eq!(
            headers
                .get("access-control-allow-headers")
                .and_then(|v| v.to_str().ok()),
            Some("Content-Type, Authorization")
        );
        assert_eq!(
            headers
                .get("access-control-max-age")
                .and_then(|v| v.to_str().ok()),
            Some("86400")
        );
        assert_eq!(
            headers.get("vary").and_then(|v| v.to_str().ok()),
            Some("Origin, Access-Control-Request-Method, Access-Control-Request-Headers")
        );
    }
    // @cpt-end:cpt-cf-oagw-dod-cors-preflight:p1:inst-cors-preflight-response-test-01
}
