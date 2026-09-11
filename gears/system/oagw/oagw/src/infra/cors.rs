//! Built-in CORS handling (ADR-0004).
//!
//! Preflight is answered locally with a permissive `204` before any upstream
//! resolution — browsers send no credentials on preflight, so there is no
//! tenant context to resolve one with. Origin and method enforcement therefore
//! happens on the *actual* request, after the upstream is known.

use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};

use crate::domain::error::{ErrorKind, OagwError, OagwResult};
use crate::domain::model::CorsConfig;

pub const PREFLIGHT_MAX_AGE_SECONDS: u32 = 86_400;

/// A request is a CORS preflight when it is `OPTIONS` and carries both
/// `Origin` and `Access-Control-Request-Method` (WHATWG Fetch).
#[must_use]
pub fn is_preflight(method: &Method, headers: &HeaderMap) -> bool {
    method == Method::OPTIONS
        && headers.contains_key(http::header::ORIGIN)
        && headers.contains_key(http::header::ACCESS_CONTROL_REQUEST_METHOD)
}

/// Headers of the permissive `204` preflight answer.
#[must_use]
pub fn preflight_headers(request_headers: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    if let Some(origin) = request_headers.get(http::header::ORIGIN) {
        out.insert(http::header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
    }
    if let Some(method) = request_headers.get(http::header::ACCESS_CONTROL_REQUEST_METHOD) {
        out.insert(http::header::ACCESS_CONTROL_ALLOW_METHODS, method.clone());
    }
    if let Some(headers) = request_headers.get(http::header::ACCESS_CONTROL_REQUEST_HEADERS) {
        out.insert(http::header::ACCESS_CONTROL_ALLOW_HEADERS, headers.clone());
    }
    if let Ok(value) = HeaderValue::from_str(&PREFLIGHT_MAX_AGE_SECONDS.to_string()) {
        out.insert(http::header::ACCESS_CONTROL_MAX_AGE, value);
    }
    out.insert(
        http::header::VARY,
        HeaderValue::from_static(
            "Origin, Access-Control-Request-Method, Access-Control-Request-Headers",
        ),
    );
    out
}

/// The preflight status code.
#[must_use]
pub fn preflight_status() -> StatusCode {
    StatusCode::NO_CONTENT
}

/// Enforce origin and method on an actual cross-origin request.
///
/// # Errors
/// `403` with the CORS-specific GTS type when the origin or the method is not
/// allowed.
pub fn validate_actual(cors: &CorsConfig, origin: &str, method: &str) -> OagwResult<()> {
    if !cors.enabled {
        return Ok(());
    }
    if !cors.allows_origin(origin) {
        return Err(OagwError::new(
            ErrorKind::CorsOriginNotAllowed,
            format!("Origin '{origin}' not in allowed origins list"),
        ));
    }
    if !cors.allows_method(method) {
        return Err(OagwError::new(
            ErrorKind::CorsMethodNotAllowed,
            format!("Method '{method}' not in allowed methods list"),
        ));
    }
    Ok(())
}

/// CORS response headers for an allowed actual request.
///
/// `Vary: Origin` is always emitted to stop a shared cache from serving one
/// origin's response to another.
#[must_use]
pub fn response_headers(cors: &CorsConfig, origin: &str) -> HeaderMap {
    let mut out = HeaderMap::new();
    if !cors.enabled {
        return out;
    }
    // With credentials the wildcard is illegal, so echo the concrete origin.
    let allow_origin = if cors.allow_credentials || !cors.allowed_origins.iter().any(|o| o == "*") {
        origin.to_owned()
    } else {
        "*".to_owned()
    };
    if let Ok(value) = HeaderValue::from_str(&allow_origin) {
        out.insert(http::header::ACCESS_CONTROL_ALLOW_ORIGIN, value);
    }
    if cors.allow_credentials {
        out.insert(
            http::header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
            HeaderValue::from_static("true"),
        );
    }
    if !cors.expose_headers.is_empty()
        && let Ok(value) = HeaderValue::from_str(&cors.expose_headers.join(", "))
    {
        out.insert(http::header::ACCESS_CONTROL_EXPOSE_HEADERS, value);
    }
    out.insert(http::header::VARY, HeaderValue::from_static("Origin"));
    out
}

/// Merge `extra` into `target`, overwriting on conflict.
pub fn merge_into(target: &mut HeaderMap, extra: HeaderMap) {
    for (name, value) in &extra {
        let name: HeaderName = name.clone();
        target.insert(name, value.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::SharingMode;

    fn cors(origins: &[&str], methods: &[&str], credentials: bool) -> CorsConfig {
        CorsConfig {
            sharing: SharingMode::Private,
            enabled: true,
            allowed_origins: origins.iter().map(|s| (*s).to_owned()).collect(),
            allowed_methods: methods.iter().map(|s| (*s).to_owned()).collect(),
            expose_headers: vec!["X-Request-ID".to_owned()],
            allow_credentials: credentials,
        }
    }

    #[test]
    fn preflight_detection_needs_all_three_signals() {
        let mut headers = HeaderMap::new();
        assert!(!is_preflight(&Method::OPTIONS, &headers));
        headers.insert(http::header::ORIGIN, HeaderValue::from_static("https://a"));
        assert!(!is_preflight(&Method::OPTIONS, &headers));
        headers.insert(
            http::header::ACCESS_CONTROL_REQUEST_METHOD,
            HeaderValue::from_static("POST"),
        );
        assert!(is_preflight(&Method::OPTIONS, &headers));
        assert!(!is_preflight(&Method::GET, &headers));
    }

    #[test]
    fn preflight_echoes_the_request_and_is_204() {
        let mut req = HeaderMap::new();
        req.insert(
            http::header::ORIGIN,
            HeaderValue::from_static("https://app.example.com"),
        );
        req.insert(
            http::header::ACCESS_CONTROL_REQUEST_METHOD,
            HeaderValue::from_static("POST"),
        );
        req.insert(
            http::header::ACCESS_CONTROL_REQUEST_HEADERS,
            HeaderValue::from_static("Content-Type, Authorization"),
        );
        let out = preflight_headers(&req);
        assert_eq!(preflight_status(), StatusCode::NO_CONTENT);
        assert_eq!(
            out.get(http::header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|v| v.to_str().ok()),
            Some("https://app.example.com")
        );
        assert_eq!(
            out.get(http::header::ACCESS_CONTROL_ALLOW_METHODS)
                .and_then(|v| v.to_str().ok()),
            Some("POST")
        );
        assert_eq!(
            out.get(http::header::ACCESS_CONTROL_ALLOW_HEADERS)
                .and_then(|v| v.to_str().ok()),
            Some("Content-Type, Authorization")
        );
        assert_eq!(
            out.get(http::header::ACCESS_CONTROL_MAX_AGE)
                .and_then(|v| v.to_str().ok()),
            Some("86400")
        );
        assert!(out.contains_key(http::header::VARY));
    }

    #[test]
    fn disallowed_origin_and_method_are_403() {
        let cfg = cors(&["https://app.example.com"], &["GET", "POST"], false);
        assert!(validate_actual(&cfg, "https://app.example.com", "GET").is_ok());

        let err = validate_actual(&cfg, "https://evil.com", "GET").expect_err("origin");
        assert_eq!(err.status(), 403);
        assert!(err.kind.gts_type().ends_with("cors.origin_not_allowed.v1"));

        let err = validate_actual(&cfg, "https://app.example.com", "DELETE").expect_err("method");
        assert_eq!(err.status(), 403);
        assert!(err.kind.gts_type().ends_with("cors.method_not_allowed.v1"));
    }

    #[test]
    fn a_disabled_config_never_rejects() {
        let mut cfg = cors(&["https://app.example.com"], &["GET"], false);
        cfg.enabled = false;
        assert!(validate_actual(&cfg, "https://evil.com", "DELETE").is_ok());
        assert!(response_headers(&cfg, "https://evil.com").is_empty());
    }

    #[test]
    fn response_headers_echo_the_origin_with_credentials() {
        let cfg = cors(&["https://app.example.com"], &["GET"], true);
        let out = response_headers(&cfg, "https://app.example.com");
        assert_eq!(
            out.get(http::header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|v| v.to_str().ok()),
            Some("https://app.example.com")
        );
        assert_eq!(
            out.get(http::header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
                .and_then(|v| v.to_str().ok()),
            Some("true")
        );
        assert_eq!(
            out.get(http::header::ACCESS_CONTROL_EXPOSE_HEADERS)
                .and_then(|v| v.to_str().ok()),
            Some("X-Request-ID")
        );
        assert_eq!(
            out.get(http::header::VARY).and_then(|v| v.to_str().ok()),
            Some("Origin")
        );
    }

    #[test]
    fn a_wildcard_config_without_credentials_emits_the_wildcard() {
        let cfg = cors(&["*"], &["GET"], false);
        let out = response_headers(&cfg, "https://anything.example");
        assert_eq!(
            out.get(http::header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|v| v.to_str().ok()),
            Some("*")
        );
    }
}
