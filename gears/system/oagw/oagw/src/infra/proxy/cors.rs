//! CORS handling for the proxy data plane.
//!
//! Implements `docs/ADR/0004-cors.md`: preflight is answered permissively
//! without resolving the upstream; actual requests are validated.

use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};

use crate::domain::error::DomainError;
use crate::domain::model::CorsConfig;

/// Request headers echoed back on a preflight when none are requested.
const DEFAULT_ALLOW_HEADERS: &str = "authorization, content-type, x-oagw-target-host, x-request-id";
/// Header carrying the browser's origin.
pub const ORIGIN: &str = "origin";
/// Header carrying the preflight method.
pub const REQUEST_METHOD: &str = "access-control-request-method";
/// Header carrying the preflight headers.
pub const REQUEST_HEADERS: &str = "access-control-request-headers";
/// Response header naming the allowed origin.
pub const ALLOW_ORIGIN: &str = "access-control-allow-origin";
/// Response header naming the allowed methods.
pub const ALLOW_METHODS: &str = "access-control-allow-methods";
/// Response header naming the allowed headers.
pub const ALLOW_HEADERS: &str = "access-control-allow-headers";
/// Response header naming the exposed headers.
pub const EXPOSE_HEADERS: &str = "access-control-expose-headers";
/// Response header enabling credentialed requests.
pub const ALLOW_CREDENTIALS: &str = "access-control-allow-credentials";
/// Header telling caches that the response varies per origin.
pub const VARY: &str = "vary";

/// Methods offered when a preflight does not name one.
const DEFAULT_METHODS: &str = "GET, POST";
/// Seconds a browser may cache the preflight answer.
pub const MAX_AGE: &str = "86400";
/// Response header telling the browser how long to cache the preflight.
pub const MAX_AGE_HEADER: &str = "access-control-max-age";

/// Whether the request is a CORS preflight.
#[must_use]
pub fn is_preflight(method: &Method, headers: &HeaderMap) -> bool {
    method == Method::OPTIONS
        && headers.contains_key(ORIGIN)
        && headers.contains_key(REQUEST_METHOD)
}

/// Whether the request is cross-origin and therefore subject to CORS.
#[must_use]
pub fn is_cross_origin(headers: &HeaderMap) -> bool {
    headers.contains_key(ORIGIN)
}

/// Build the permissive preflight response headers.
#[must_use]
pub fn preflight_headers(headers: &HeaderMap, cors: Option<&CorsConfig>) -> HeaderMap {
    let mut out = HeaderMap::new();
    if let Some(origin) = headers.get(ORIGIN)
        && let Ok(value) = HeaderValue::from_bytes(origin.as_bytes())
    {
        out.insert(ALLOW_ORIGIN, value);
    }
    let requested = headers
        .get(REQUEST_METHOD)
        .and_then(|v| v.to_str().ok())
        .map_or_else(|| DEFAULT_METHODS.to_owned(), str::to_owned);
    if let Ok(value) = HeaderValue::from_str(&requested) {
        out.insert(ALLOW_METHODS, value);
    }
    let requested_headers = headers
        .get(REQUEST_HEADERS)
        .and_then(|v| v.to_str().ok())
        .map_or_else(|| DEFAULT_ALLOW_HEADERS.to_owned(), str::to_owned);
    if let Ok(value) = HeaderValue::from_str(&requested_headers) {
        out.insert(ALLOW_HEADERS, value);
    }
    if let Some(cors) = cors {
        if !cors.expose_headers.is_empty()
            && let Ok(value) = HeaderValue::from_str(&cors.expose_headers.join(", "))
        {
            out.insert(EXPOSE_HEADERS, value);
        }
        if cors.allow_credentials {
            out.insert(ALLOW_CREDENTIALS, HeaderValue::from_static("true"));
        }
    }
    out.insert(VARY, HeaderValue::from_static("Origin"));
    out.insert(MAX_AGE_HEADER, HeaderValue::from_static(MAX_AGE));
    out
}
///
/// # Errors
/// Returns [`DomainError::CorsOriginNotAllowed`] and
/// [`DomainError::CorsMethodNotAllowed`] on a violation.
/// Validate an actual cross-origin request against the CORS configuration.
///
/// # Errors
/// Returns [`DomainError::CorsOriginNotAllowed`] and
/// [`DomainError::CorsMethodNotAllowed`] on a violation.
pub fn validate_actual(
    cors: &CorsConfig,
    method: &Method,
    headers: &HeaderMap,
) -> Result<(), DomainError> {
    let origin = headers
        .get(ORIGIN)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let allowed = cors
        .allowed_origins
        .iter()
        .any(|o| o == "*" || o.eq_ignore_ascii_case(origin));
    if !allowed {
        return Err(DomainError::CorsOriginNotAllowed(origin.to_owned()));
    }
    let allowed_methods = if cors.allowed_methods.is_empty() {
        vec!["GET".to_owned(), "POST".to_owned()]
    } else {
        cors.allowed_methods.clone()
    };
    let matches = allowed_methods
        .iter()
        .any(|m| m.eq_ignore_ascii_case(method.as_str()));
    if !matches {
        return Err(DomainError::CorsMethodNotAllowed(method.to_string()));
    }
    Ok(())
}

/// Headers added to every actual response of a CORS-enabled upstream.
#[must_use]
pub fn actual_response_headers(cors: &CorsConfig, headers: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    let origin = headers.get(ORIGIN).cloned();
    if let Some(origin) = origin {
        out.insert(ALLOW_ORIGIN, origin);
    }
    let allowed_methods = if cors.allowed_methods.is_empty() {
        DEFAULT_METHODS.to_owned()
    } else {
        cors.allowed_methods.join(", ")
    };
    if let Ok(value) = HeaderValue::from_str(&allowed_methods) {
        out.insert(ALLOW_METHODS, value);
    }
    if !cors.expose_headers.is_empty() {
        let exposed = cors.expose_headers.join(", ");
        if let Ok(value) = HeaderValue::from_str(&exposed) {
            out.insert(EXPOSE_HEADERS, value);
        }
    }
    if cors.allow_credentials {
        out.insert(ALLOW_CREDENTIALS, HeaderValue::from_static("true"));
    }
    out.insert(VARY, HeaderValue::from_static("Origin"));
    out
}

/// Whether CORS applies to this request.
#[must_use]
pub fn applies(cors: Option<&CorsConfig>, headers: &HeaderMap) -> bool {
    cors.is_some_and(|c| c.enabled) && is_cross_origin(headers)
}

/// Parse a header name for the CORS tables.
///
/// # Errors
/// Returns `None` when the name is not a valid header name.
#[must_use]
pub fn header_name(name: &str) -> Option<HeaderName> {
    HeaderName::try_from(name).ok()
}

/// Build a `204 No Content` preflight response.
#[must_use]
pub fn preflight_response(
    headers: &HeaderMap,
    cors: Option<&CorsConfig>,
) -> http::Response<axum::body::Body> {
    let mut builder = http::Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header("x-oagw-error-source", "gateway");
    for (name, value) in &preflight_headers(headers, cors) {
        builder = builder.header(name, value);
    }
    builder
        .body(axum::body::Body::empty())
        .unwrap_or_else(|_| http::Response::new(axum::body::Body::empty()))
}

/// Value marking a gateway-generated response.
pub const GATEWAY_SOURCE: &str = "gateway";
/// Value marking an upstream response.
pub const UPSTREAM_SOURCE: &str = "upstream";

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn cors() -> CorsConfig {
        CorsConfig {
            enabled: true,
            allowed_origins: vec!["https://app.example.com".to_owned()],
            allowed_methods: vec!["GET".to_owned(), "POST".to_owned()],
            ..CorsConfig::default()
        }
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    #[test]
    fn preflight_is_detected_by_method_and_headers() {
        let h = headers(&[
            ("origin", "https://app.example.com"),
            ("access-control-request-method", "POST"),
        ]);
        assert!(is_preflight(&Method::OPTIONS, &h));
        assert!(!is_preflight(&Method::GET, &h));
        assert!(!is_preflight(&Method::OPTIONS, &HeaderMap::new()));
    }

    #[test]
    fn preflight_response_is_permissive() {
        let h = headers(&[
            ("origin", "https://app.example.com"),
            ("access-control-request-method", "POST"),
        ]);
        let response = preflight_response(&h, Some(&cors()));
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            response.headers().get(ALLOW_ORIGIN).unwrap(),
            "https://app.example.com"
        );
        assert_eq!(response.headers().get(ALLOW_METHODS).unwrap(), "POST");
        assert_eq!(response.headers().get(VARY).unwrap(), "Origin");
    }

    #[test]
    fn disallowed_origin_is_rejected() {
        let h = headers(&[("origin", "https://evil.com")]);
        let err = validate_actual(&cors(), &Method::GET, &h).unwrap_err();
        assert!(matches!(err, DomainError::CorsOriginNotAllowed(_)));
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            err.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1"
        );
    }

    #[test]
    fn disallowed_method_is_rejected() {
        let h = headers(&[("origin", "https://app.example.com")]);
        let err = validate_actual(&cors(), &Method::DELETE, &h);
        assert!(matches!(err, Err(DomainError::CorsMethodNotAllowed(_))));
    }

    #[test]
    fn allowed_origin_and_method_pass() {
        let h = headers(&[("origin", "https://app.example.com")]);
        assert!(validate_actual(&cors(), &Method::GET, &h).is_ok());
    }

    #[test]
    fn actual_response_headers_echo_the_origin() {
        let h = headers(&[("origin", "https://app.example.com")]);
        let out = actual_response_headers(&cors(), &h);
        assert_eq!(out.get(ALLOW_ORIGIN).unwrap(), "https://app.example.com");
        assert_eq!(out.get(VARY).unwrap(), "Origin");
    }

    #[test]
    fn cors_applies_only_when_enabled_and_cross_origin() {
        let h = headers(&[("origin", "https://app.example.com")]);
        assert!(applies(Some(&cors()), &h));
        assert!(!applies(Some(&cors()), &HeaderMap::new()));
        let mut disabled = cors();
        disabled.enabled = false;
        assert!(!applies(Some(&disabled), &h));
    }
}
