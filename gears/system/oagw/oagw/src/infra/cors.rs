//! Built-in CORS handling (ADR 0004).
//!
//! * Preflight detection (`OPTIONS` + `Origin` + `Access-Control-Request-
//!   Method`) and the permissive 204 response live in the proxy handler:
//!   they run before any upstream resolution and need no tenant context.
//! * [`enforce_actual`] validates the requested origin and method against
//!   the binding's [`CorsConfig`] — actual cross-origin requests only — and
//!   rejects with 403 (`cf.oagw.cors.origin_not_allowed.v1` /
//!   `method_not_allowed.v1`).
//! * [`preflight_headers`] / [`apply_response_headers`] decorate preflight
//!   204s and proxied responses. `Vary: Origin` is always present to
//!   prevent cache poisoning.

use http::header::{
    HeaderMap, HeaderValue, ACCESS_CONTROL_ALLOW_CREDENTIALS, ACCESS_CONTROL_ALLOW_HEADERS,
    ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN, ACCESS_CONTROL_EXPOSE_HEADERS,
    ACCESS_CONTROL_MAX_AGE, ACCESS_CONTROL_REQUEST_HEADERS, ACCESS_CONTROL_REQUEST_METHOD,
    ORIGIN, VARY,
};

use crate::domain::error::{DataPlaneError, ErrorExtensions};
use crate::domain::models::CorsConfig;

/// Preflight cache lifetime (`Access-Control-Max-Age`), per ADR 0004.
pub const PREFLIGHT_MAX_AGE: u64 = 86_400;

/// Detect a CORS preflight request (ADR 0004): `OPTIONS` carrying both
/// `Origin` and `Access-Control-Request-Method`.
#[must_use]
pub fn is_preflight(method: &http::Method, headers: &HeaderMap) -> bool {
    *method == http::Method::OPTIONS
        && headers.contains_key(ORIGIN)
        && headers.contains_key(ACCESS_CONTROL_REQUEST_METHOD)
}

/// Response headers for a permissive preflight 204, echoing the requested
/// origin, method, and headers back (ADR 0004).
#[must_use]
pub fn preflight_headers(request_headers: &HeaderMap) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Some(origin) = request_headers.get(ORIGIN) {
        headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
    }
    if let Some(method) = request_headers.get(ACCESS_CONTROL_REQUEST_METHOD) {
        headers.insert(ACCESS_CONTROL_ALLOW_METHODS, method.clone());
    }
    if let Some(request_headers) = request_headers.get(ACCESS_CONTROL_REQUEST_HEADERS) {
        headers.insert(ACCESS_CONTROL_ALLOW_HEADERS, request_headers.clone());
    }
    headers.insert(
        ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static("86400"),
    );
    headers.insert(
        VARY,
        HeaderValue::from_static(
            "Origin, Access-Control-Request-Method, Access-Control-Request-Headers",
        ),
    );
    headers
}

/// Headers to attach to the proxied response of an allowed actual request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorsResponseHeaders {
    /// Value for `Access-Control-Allow-Origin` (matched origin or `*`).
    pub allow_origin: String,
    /// `Access-Control-Allow-Credentials: true` (wildcard origin forbids
    /// credentials, so this is mutually exclusive with `allow_origin="*"`).
    pub allow_credentials: bool,
    /// `Access-Control-Expose-Headers` when `expose_headers` configured.
    pub expose_headers: Option<String>,
}

/// Validate an actual cross-origin request against the binding's CORS
/// config. Returns `Ok(None)` when CORS is disabled or the request carries
/// no `Origin` (not cross-origin — no enforcement, no headers).
///
/// # Errors
/// `CorsOriginNotAllowed` / `CorsMethodNotAllowed` (403) per ADR 0004.
#[allow(clippy::result_large_err)] // rich RFC 9457 error carrier by design
pub fn enforce_actual(
    origin: Option<&str>,
    method: &str,
    config: &CorsConfig,
) -> Result<Option<CorsResponseHeaders>, DataPlaneError> {
    if !config.enabled {
        return Ok(None);
    }
    let Some(origin) = origin else {
        // Same-origin / non-browser request: CORS does not apply.
        return Ok(None);
    };
    let allowed = config.allowed_origins.iter().any(|o| o == "*" || o == origin);
    if !allowed {
        return Err(DataPlaneError::CorsOriginNotAllowed {
            origin: origin.to_owned(),
            extensions: ErrorExtensions::default(),
        });
    }
    if !config.allowed_methods.iter().any(|m| m.as_str() == method) {
        return Err(DataPlaneError::CorsMethodNotAllowed {
            method: method.to_owned(),
            extensions: ErrorExtensions::default(),
        });
    }
    let allow_origin = if config.allowed_origins.iter().any(|o| o == "*") {
        "*".to_owned()
    } else {
        origin.to_owned()
    };
    let expose_headers = (!config.expose_headers.is_empty())
        .then(|| config.expose_headers.join(", "));
    Ok(Some(CorsResponseHeaders {
        allow_origin,
        allow_credentials: config.allow_credentials,
        expose_headers,
    }))
}

/// Apply the CORS response headers on the proxied response.
pub fn apply_response_headers(headers: &mut HeaderMap, cors: &CorsResponseHeaders) {
    if let Ok(value) = HeaderValue::from_str(&cors.allow_origin) {
        headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, value);
    }
    if let Some(value) = cors
        .expose_headers
        .as_deref()
        .and_then(|e| HeaderValue::from_str(e).ok())
    {
        headers.insert(ACCESS_CONTROL_EXPOSE_HEADERS, value);
    }
    if cors.allow_credentials {
        headers.insert(
            ACCESS_CONTROL_ALLOW_CREDENTIALS,
            HeaderValue::from_static("true"),
        );
    }
    // Note: literal canonical name (http's ORIGIN.as_str() is lowercase).
    append_vary(headers, "Origin");
}

/// Validate a merged CORS config (callers do this at merge time):
/// `allow_credentials` with a wildcard origin is invalid (ADR 0004).
///
/// # Errors
/// Returns the config unchanged on success; a description otherwise.
pub fn validate_config(config: &CorsConfig) -> Result<(), String> {
    if config.enabled
        && config.allow_credentials
        && config.allowed_origins.iter().any(|o| o == "*")
    {
        return Err("Cannot use allow_credentials with wildcard origin".to_owned());
    }
    Ok(())
}

/// Append `Vary: <name>` to the response without duplicating it.
pub(crate) fn append_vary(headers: &mut HeaderMap, name: &str) {
    // Preserve comma-separated values across re-applications.
    if let Some(existing) = headers.get(VARY).and_then(|v| v.to_str().ok()) {
        if existing.split(',').any(|v| v.trim().eq_ignore_ascii_case(name)) {
            return;
        }
        let value = format!("{existing}, {name}");
        if let Ok(value) = HeaderValue::from_str(&value) {
            headers.insert(VARY, value);
        }
        return;
    }
    if let Ok(value) = HeaderValue::from_str(name) {
        headers.insert(VARY, value);
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::models::CorsMethod;
    use http::header::HeaderMap;
    use http::Method;

    fn cfg(origins: &[&str], methods: &[CorsMethod], credentials: bool) -> CorsConfig {
        CorsConfig {
            sharing: crate::domain::models::SharingMode::Inherit,
            enabled: true,
            allowed_origins: origins.iter().map(|s| (*s).to_owned()).collect(),
            allowed_methods: methods.to_vec(),
            expose_headers: vec!["X-Request-ID".to_owned()],
            allow_credentials: credentials,
        }
    }

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_static(v));
        }
        h
    }

    #[test]
    fn preflight_detection_requires_all_three_markers() {
        let pre = headers(&[
            ("origin", "https://app.example.com"),
            ("access-control-request-method", "POST"),
        ]);
        assert!(is_preflight(&Method::OPTIONS, &pre));
        // Missing origin.
        let no_origin = headers(&[("access-control-request-method", "POST")]);
        assert!(!is_preflight(&Method::OPTIONS, &no_origin));
        // Missing method marker.
        let no_method = headers(&[("origin", "https://app.example.com")]);
        assert!(!is_preflight(&Method::OPTIONS, &no_method));
        // Non-OPTIONS.
        assert!(!is_preflight(&Method::GET, &pre));
    }

    #[test]
    fn preflight_headers_echo_and_vary() {
        let req = headers(&[
            ("origin", "https://app.example.com"),
            ("access-control-request-method", "POST"),
            ("access-control-request-headers", "content-type, authorization"),
        ]);
        let h = preflight_headers(&req);
        assert_eq!(h["access-control-allow-origin"], "https://app.example.com");
        assert_eq!(h["access-control-allow-methods"], "POST");
        assert_eq!(
            h["access-control-allow-headers"],
            "content-type, authorization"
        );
        assert_eq!(h["access-control-max-age"], "86400");
        assert_eq!(
            h["vary"],
            "Origin, Access-Control-Request-Method, Access-Control-Request-Headers"
        );
    }

    #[test]
    fn disabled_or_non_cross_origin_are_noops() {
        let config = cfg(&["https://app.example.com"], &[CorsMethod::Get], false);
        assert_eq!(enforce_actual(None, "GET", &config).unwrap(), None);
        let mut disabled = config;
        disabled.enabled = false;
        assert_eq!(
            enforce_actual(Some("https://evil.com"), "GET", &disabled).unwrap(),
            None
        );
    }

    #[test]
    fn exact_origin_matching_is_port_and_protocol_sensitive() {
        let config = cfg(&["https://app.example.com"], &[CorsMethod::Get], false);
        // Allowed.
        assert!(enforce_actual(Some("https://app.example.com"), "GET", &config)
            .unwrap()
            .is_some());
        // Port differs.
        let err = enforce_actual(Some("https://app.example.com:8443"), "GET", &config)
            .unwrap_err();
        assert!(matches!(err, DataPlaneError::CorsOriginNotAllowed { .. }));
        // Protocol differs.
        assert!(matches!(
            enforce_actual(Some("http://app.example.com"), "GET", &config).unwrap_err(),
            DataPlaneError::CorsOriginNotAllowed { .. }
        ));
        // Substring trick does not match.
        assert!(matches!(
            enforce_actual(Some("https://evil.com.example.com"), "GET", &config).unwrap_err(),
            DataPlaneError::CorsOriginNotAllowed { .. }
        ));
    }

    #[test]
    fn method_not_allowed_rejects_403() {
        let config = cfg(&["https://app.example.com"], &[CorsMethod::Get], false);
        assert!(matches!(
            enforce_actual(Some("https://app.example.com"), "POST", &config).unwrap_err(),
            DataPlaneError::CorsMethodNotAllowed { .. }
        ));
    }

    #[test]
    fn wildcard_origin_echoes_star_without_credentials() {
        let config = cfg(&["*"], &[CorsMethod::Get, CorsMethod::Post], false);
        let cors = enforce_actual(Some("https://anywhere.io"), "POST", &config)
            .unwrap()
            .unwrap();
        assert_eq!(cors.allow_origin, "*");
        assert!(!cors.allow_credentials);
    }

    #[test]
    fn credentials_with_wildcard_is_invalid() {
        let config = cfg(&["*"], &[CorsMethod::Get], true);
        assert!(validate_config(&config).is_err());
        // Credentials with explicit origins are fine.
        let explicit = cfg(&["https://app.example.com"], &[CorsMethod::Get], true);
        assert!(validate_config(&explicit).is_ok());
    }

    #[test]
    fn response_headers_and_vary_append() {
        let cors = CorsResponseHeaders {
            allow_origin: "https://app.example.com".to_owned(),
            allow_credentials: true,
            expose_headers: Some("X-Request-ID".to_owned()),
        };
        let mut h = HeaderMap::new();
        apply_response_headers(&mut h, &cors);
        assert_eq!(h["access-control-allow-origin"], "https://app.example.com");
        assert_eq!(h["access-control-allow-credentials"], "true");
        assert_eq!(h["access-control-expose-headers"], "X-Request-ID");
        assert_eq!(h["vary"], "Origin");
        // Re-applying does not duplicate Vary.
        apply_response_headers(&mut h, &cors);
        assert_eq!(h["vary"], "Origin");
    }
}
