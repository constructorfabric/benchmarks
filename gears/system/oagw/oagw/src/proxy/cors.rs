//! Built-in CORS handling (ADR 0004).
//!
//! Preflight (`OPTIONS` + `Origin` + `Access-Control-Request-Method`) is
//! answered permissively at the handler level — the browser sends no
//! credentials on a preflight, so no tenant context exists to resolve an
//! upstream against. Origin and method enforcement happens on the *actual*
//! request, after upstream resolution and before forwarding.

use axum::body::Body;
use axum::http::{HeaderName, HeaderValue, Method, header};
use axum::response::Response;

use crate::domain::error::{DomainError, ErrorKind};
use crate::domain::model::CorsConfig;

/// `Vary` value always emitted to prevent cache poisoning.
pub const VARY_VALUE: &str = "Origin";

/// Preflight cache lifetime advertised by the handler-level preflight (ADR 0004).
pub const DEFAULT_PREFLIGHT_MAX_AGE_SECS: u64 = 86_400;

/// `true` when the request is a CORS preflight per ADR 0004.
#[must_use]
pub fn is_preflight(method: &Method, origin: Option<&str>, request_method: Option<&str>) -> bool {
    method == Method::OPTIONS
        && origin.is_some_and(|o| !o.trim().is_empty())
        && request_method.is_some_and(|m| !m.trim().is_empty())
}

/// Builds the permissive 204 preflight response.
///
/// ADR 0004: the preflight echoes the requested origin, method and headers, so
/// `request_headers` (the raw `Access-Control-Request-Headers` value) is
/// reflected back verbatim; the configured allow-list is only a fallback.
#[must_use]
pub fn preflight_response(
    cors: &CorsConfig,
    origin: &str,
    request_method: &str,
    request_headers: Option<&str>,
) -> Response {
    let mut builder = Response::builder()
        .status(http::StatusCode::NO_CONTENT)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin)
        .header(header::ACCESS_CONTROL_ALLOW_METHODS, request_method)
        .header(header::VARY, VARY_VALUE);

    let allow_headers = request_headers
        .map(str::to_owned)
        .or_else(|| (!cors.allowed_headers.is_empty()).then(|| cors.allowed_headers.join(", ")));
    if let Some(value) = allow_headers.filter(|value| !value.trim().is_empty()) {
        builder = builder.header(header::ACCESS_CONTROL_ALLOW_HEADERS, value);
    }
    let max_age = cors.max_age.unwrap_or(DEFAULT_PREFLIGHT_MAX_AGE_SECS);
    builder = builder.header(header::ACCESS_CONTROL_MAX_AGE, max_age.to_string());
    builder
        .body(Body::empty())
        .unwrap_or_else(|_| axum::http::Response::new(Body::empty()))
}

/// Validates an actual cross-origin request against the effective CORS config.
///
/// Same-origin requests (no `Origin` header) are never CORS requests and pass.
///
/// # Errors
///
/// Returns `CorsOriginNotAllowed` / `CorsMethodNotAllowed` when the origin or
/// method falls outside the configured allow-lists.
pub fn check_actual_request(
    cors: &CorsConfig,
    origin: Option<&str>,
    method: &Method,
) -> Result<(), DomainError> {
    let Some(origin) = origin else {
        return Ok(());
    };
    if origin.trim().is_empty() || !cors.enabled {
        return Ok(());
    }
    if !origin_allowed(cors, origin) {
        return Err(DomainError::new(
            ErrorKind::CorsOriginNotAllowed,
            format!("Origin '{origin}' not in allowed origins list"),
        )
        .with_extension("origin", serde_json::json!(origin)));
    }
    if !method_allowed(cors, method) {
        return Err(DomainError::new(
            ErrorKind::CorsMethodNotAllowed,
            format!("Method '{method}' not in allowed methods list"),
        )
        .with_extension("method", serde_json::json!(method.as_str())));
    }
    Ok(())
}

/// `true` when `origin` is listed (exact, case-insensitive) or the config
/// carries the wildcard.
#[must_use]
pub fn origin_allowed(cors: &CorsConfig, origin: &str) -> bool {
    cors.allowed_origins
        .iter()
        .any(|allowed| allowed == "*" || allowed.eq_ignore_ascii_case(origin))
}

/// `true` when the method is listed, or when no allow-list is configured.
///
/// An empty `allowed_methods` is treated as "no CORS method restriction" so a
/// config that only pins origins does not reject every request.
#[must_use]
pub fn method_allowed(cors: &CorsConfig, method: &Method) -> bool {
    if cors.allowed_methods.is_empty() {
        return true;
    }
    cors.allowed_methods
        .iter()
        .any(|m| m.eq_ignore_ascii_case(method.as_str()))
}

/// Adds the CORS response headers for an allowed actual request.
pub fn apply_response_headers(headers: &mut http::HeaderMap, origin: &str, cors: &CorsConfig) {
    if let Ok(value) = HeaderValue::from_str(origin) {
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, value);
    }
    if !cors.expose_headers.is_empty()
        && let Ok(value) = HeaderValue::from_str(&cors.expose_headers.join(", "))
    {
        headers.insert(header::ACCESS_CONTROL_EXPOSE_HEADERS, value);
    }
    if cors.allow_credentials {
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
            HeaderValue::from_static("true"),
        );
    }
    append_vary(headers, HeaderValue::from_static(VARY_VALUE));
}

/// Appends `Origin` to an existing `Vary` header without duplicating it.
fn append_vary(headers: &mut http::HeaderMap, value: HeaderValue) {
    let present = headers.get_all(header::VARY).iter().any(|existing| {
        existing
            .to_str()
            .is_ok_and(|v| v.eq_ignore_ascii_case("origin"))
    });
    if present {
        return;
    }
    headers.append(header::VARY, value);
}

/// Header name accessor used by the proxy handler for the preflight check.
#[must_use]
pub fn header_value<'a>(headers: &'a http::HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// Normalises a header name for diagnostics (kept for symmetry with the
/// `Access-Control-Request-Headers` echo).
#[must_use]
pub fn normalize_header_name(name: &str) -> Option<HeaderName> {
    HeaderName::from_bytes(name.trim().as_bytes()).ok()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::model::SharingMode;

    fn cors(origins: &[&str], methods: &[&str]) -> CorsConfig {
        CorsConfig {
            sharing: SharingMode::Private,
            enabled: true,
            allowed_origins: origins.iter().map(|s| (*s).to_owned()).collect(),
            allowed_methods: methods.iter().map(|s| (*s).to_owned()).collect(),
            ..CorsConfig::default()
        }
    }

    #[test]
    fn preflight_detection_requires_origin_and_request_method() {
        assert!(is_preflight(
            &Method::OPTIONS,
            Some("https://app.example"),
            Some("POST")
        ));
        assert!(!is_preflight(
            &Method::POST,
            Some("https://a.example"),
            Some("POST")
        ));
        assert!(!is_preflight(&Method::OPTIONS, None, Some("POST")));
        assert!(!is_preflight(
            &Method::OPTIONS,
            Some("https://a.example"),
            None
        ));
        assert!(!is_preflight(&Method::OPTIONS, Some("  "), Some("POST")));
    }

    #[test]
    fn origin_matching_is_exact_and_protocol_sensitive() {
        let cfg = cors(&["https://app.example.com"], &[]);
        assert!(origin_allowed(&cfg, "https://app.example.com"));
        assert!(!origin_allowed(&cfg, "https://app.example.com:8080"));
        assert!(!origin_allowed(&cfg, "http://app.example.com"));
        assert!(!origin_allowed(&cfg, "https://evil.com.example.com"));
        assert!(origin_allowed(
            &cors(&["*"], &[]),
            "https://anything.example"
        ));
    }

    #[test]
    fn empty_method_list_does_not_restrict() {
        let cfg = cors(&["https://app.example.com"], &[]);
        assert!(method_allowed(&cfg, &Method::DELETE));
        let strict = cors(&["https://app.example.com"], &["GET"]);
        assert!(method_allowed(&strict, &Method::GET));
        assert!(!method_allowed(&strict, &Method::DELETE));
    }

    #[test]
    fn actual_request_rejections_carry_the_adr_gts_types() {
        let cfg = cors(&["https://app.example.com"], &["GET"]);
        let origin_err = check_actual_request(&cfg, Some("https://evil.com"), &Method::GET)
            .expect_err("origin rejected");
        assert_eq!(origin_err.kind, ErrorKind::CorsOriginNotAllowed);
        let method_err = check_actual_request(&cfg, Some("https://app.example.com"), &Method::POST)
            .expect_err("method rejected");
        assert_eq!(method_err.kind, ErrorKind::CorsMethodNotAllowed);
        assert!(check_actual_request(&cfg, None, &Method::POST).is_ok());
    }

    #[test]
    fn preflight_response_echoes_the_request() {
        let mut cfg = cors(&["https://app.example.com"], &["POST"]);
        cfg.allowed_headers = vec!["Content-Type".to_owned()];
        cfg.max_age = Some(86_400);
        let response = preflight_response(
            &cfg,
            "https://app.example.com",
            "POST",
            Some("Content-Type, Authorization"),
        );
        assert_eq!(response.status(), http::StatusCode::NO_CONTENT);
        let headers = response.headers();
        assert_eq!(
            headers.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(),
            "https://app.example.com"
        );
        assert_eq!(
            headers.get(header::ACCESS_CONTROL_ALLOW_HEADERS).unwrap(),
            "Content-Type, Authorization",
            "the requested headers are echoed verbatim"
        );
        assert_eq!(
            headers.get(header::ACCESS_CONTROL_MAX_AGE).unwrap(),
            "86400"
        );
        assert!(headers.get(header::VARY).is_some());

        let response = preflight_response(&cfg, "https://app.example.com", "POST", None);
        assert_eq!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_HEADERS)
                .unwrap(),
            "Content-Type",
            "the configured allow-list is the fallback"
        );
        let response = preflight_response(
            &CorsConfig::default(),
            "https://app.example.com",
            "GET",
            None,
        );
        assert!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_HEADERS)
                .is_none()
        );
    }

    #[test]
    fn response_headers_do_not_duplicate_vary() {
        let cfg = cors(&["https://app.example.com"], &["GET"]);
        let mut headers = http::HeaderMap::new();
        apply_response_headers(&mut headers, "https://app.example.com", &cfg);
        apply_response_headers(&mut headers, "https://app.example.com", &cfg);
        assert_eq!(
            headers.get_all(header::VARY).iter().count(),
            1,
            "Vary: Origin must not be appended twice"
        );
        assert_eq!(
            headers.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(),
            "https://app.example.com"
        );
    }

    #[test]
    fn header_name_normalisation_is_total() {
        assert!(normalize_header_name("content-type").is_some());
        assert!(normalize_header_name("bad header").is_none());
    }
}
