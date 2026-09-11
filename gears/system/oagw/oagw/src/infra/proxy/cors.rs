//! Built-in CORS handling (`cpt-cf-oagw-adr-cors`).
//!
//! Two paths, deliberately asymmetric:
//!
//! * **Preflight** — a browser preflight carries no credentials, so there is
//!   no tenant context and no upstream to resolve. The handler answers `204`
//!   immediately, echoing the requested origin, method and headers.
//! * **Actual request** — after the upstream is resolved, the origin and the
//!   method are validated against the effective policy and a violation is
//!   refused with `403` *before* anything is forwarded.
//!
//! `Vary: Origin` is always emitted so a shared cache cannot serve one
//! origin's response to another.

use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::Response;

use crate::domain::error::{ERROR_SOURCE_GATEWAY, ERROR_SOURCE_HEADER, ErrorKind, OagwError};
use crate::domain::model::CorsConfig;

/// `Access-Control-Max-Age` for preflight responses, in seconds.
const PREFLIGHT_MAX_AGE: &str = "86400";

/// Whether this request is a CORS preflight: `OPTIONS` plus `Origin` plus
/// `Access-Control-Request-Method` (WHATWG Fetch).
#[must_use]
pub fn is_preflight(method: &Method, headers: &HeaderMap) -> bool {
    *method == Method::OPTIONS
        && headers.contains_key(header::ORIGIN)
        && headers.contains_key(header::ACCESS_CONTROL_REQUEST_METHOD)
}

/// Build the permissive `204` preflight response.
#[must_use]
pub fn preflight_response(headers: &HeaderMap) -> Response {
    let mut response = Response::new(axum::body::Body::empty());
    *response.status_mut() = StatusCode::NO_CONTENT;
    let out = response.headers_mut();
    if let Some(origin) = headers.get(header::ORIGIN) {
        out.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
    }
    if let Some(method) = headers.get(header::ACCESS_CONTROL_REQUEST_METHOD) {
        out.insert(header::ACCESS_CONTROL_ALLOW_METHODS, method.clone());
    }
    if let Some(request_headers) = headers.get(header::ACCESS_CONTROL_REQUEST_HEADERS) {
        out.insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            request_headers.clone(),
        );
    }
    out.insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static(PREFLIGHT_MAX_AGE),
    );
    out.insert(
        header::VARY,
        HeaderValue::from_static(
            "Origin, Access-Control-Request-Method, Access-Control-Request-Headers",
        ),
    );
    out.insert(
        HeaderName::from_static(ERROR_SOURCE_HEADER),
        HeaderValue::from_static(ERROR_SOURCE_GATEWAY),
    );
    response
}

/// Validate an actual (non-preflight) cross-origin request.
///
/// A request without an `Origin` header is not a browser cross-origin request
/// and is left alone. When CORS is disabled for the upstream, nothing is
/// enforced and nothing is added — secure by default means "no CORS story",
/// not "deny everything".
///
/// # Errors
///
/// `403` when the origin or the method is not allowed.
pub fn check_actual_request(
    config: Option<&CorsConfig>,
    method: &Method,
    headers: &HeaderMap,
) -> Result<(), OagwError> {
    let Some(config) = config else {
        return Ok(());
    };
    if !config.enabled {
        return Ok(());
    }
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return Ok(());
    };
    if !config.origin_allowed(origin) {
        return Err(OagwError::new(
            ErrorKind::CorsOriginNotAllowed,
            format!("Origin '{origin}' not in allowed origins list"),
        )
        .with("origin", origin.to_owned()));
    }
    let methods = config.methods();
    if !methods.iter().any(|allowed| allowed == method.as_str()) {
        return Err(OagwError::new(
            ErrorKind::CorsMethodNotAllowed,
            format!("Method '{method}' not in allowed methods list"),
        )
        .with("method", method.as_str().to_owned()));
    }
    Ok(())
}

/// Add the CORS response headers for an allowed actual request.
pub fn apply_response_headers(
    out: &mut HeaderMap,
    config: Option<&CorsConfig>,
    request_headers: &HeaderMap,
) {
    // `Vary: Origin` protects caches even when CORS is off, because whether a
    // response carries CORS headers depends on the request's origin.
    out.insert(header::VARY, HeaderValue::from_static("Origin"));
    let Some(config) = config else { return };
    if !config.enabled {
        return;
    }
    let Some(origin) = request_headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
    else {
        return;
    };
    if !config.origin_allowed(origin) {
        return;
    }
    // Echo the concrete origin rather than `*` so the response stays valid
    // when credentials are in play and caches stay correctly keyed.
    if let Ok(value) = HeaderValue::from_str(origin) {
        out.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, value);
    }
    if !config.expose_headers.is_empty()
        && let Ok(value) = HeaderValue::from_str(&config.expose_headers.join(", "))
    {
        out.insert(header::ACCESS_CONTROL_EXPOSE_HEADERS, value);
    }
    if config.allow_credentials {
        out.insert(
            header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
            HeaderValue::from_static("true"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::SharingMode;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                HeaderName::try_from(*name).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    fn config(origins: &[&str], methods: Option<&[&str]>, credentials: bool) -> CorsConfig {
        CorsConfig {
            sharing: SharingMode::Private,
            enabled: true,
            allowed_origins: origins.iter().map(|o| (*o).to_owned()).collect(),
            allowed_methods: methods.map(|m| m.iter().map(|m| (*m).to_owned()).collect()),
            expose_headers: vec!["X-Request-ID".to_owned()],
            allow_credentials: credentials,
        }
    }

    #[test]
    fn preflight_needs_options_origin_and_request_method() {
        let full = headers(&[
            ("origin", "https://app.example.com"),
            ("access-control-request-method", "POST"),
        ]);
        assert!(is_preflight(&Method::OPTIONS, &full));
        assert!(!is_preflight(&Method::POST, &full));
        assert!(!is_preflight(
            &Method::OPTIONS,
            &headers(&[("origin", "https://app.example.com")])
        ));
    }

    #[test]
    fn preflight_echoes_the_request_and_is_tagged_as_gateway() {
        let request = headers(&[
            ("origin", "https://app.example.com"),
            ("access-control-request-method", "POST"),
            (
                "access-control-request-headers",
                "Content-Type, Authorization",
            ),
        ]);
        let response = preflight_response(&request);
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let out = response.headers();
        assert_eq!(
            out.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(),
            "https://app.example.com"
        );
        assert_eq!(
            out.get(header::ACCESS_CONTROL_ALLOW_METHODS).unwrap(),
            "POST"
        );
        assert_eq!(
            out.get(header::ACCESS_CONTROL_ALLOW_HEADERS).unwrap(),
            "Content-Type, Authorization"
        );
        assert_eq!(out.get(header::ACCESS_CONTROL_MAX_AGE).unwrap(), "86400");
        assert!(
            out.get(header::VARY)
                .unwrap()
                .to_str()
                .unwrap()
                .contains("Origin")
        );
        assert_eq!(out.get(ERROR_SOURCE_HEADER).unwrap(), ERROR_SOURCE_GATEWAY);
    }

    #[test]
    fn disallowed_origin_is_a_403() {
        let cfg = config(&["https://app.example.com"], None, false);
        let err = check_actual_request(
            Some(&cfg),
            &Method::GET,
            &headers(&[("origin", "https://evil.com")]),
        )
        .expect_err("rejected");
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
        assert_eq!(err.kind(), ErrorKind::CorsOriginNotAllowed);
    }

    #[test]
    fn disallowed_method_is_a_403() {
        let cfg = config(&["https://app.example.com"], Some(&["GET"]), false);
        let err = check_actual_request(
            Some(&cfg),
            &Method::DELETE,
            &headers(&[("origin", "https://app.example.com")]),
        )
        .expect_err("rejected");
        assert_eq!(err.kind(), ErrorKind::CorsMethodNotAllowed);
    }

    #[test]
    fn requests_without_an_origin_are_untouched() {
        let cfg = config(&["https://app.example.com"], Some(&["GET"]), false);
        assert!(check_actual_request(Some(&cfg), &Method::DELETE, &HeaderMap::new()).is_ok());
    }

    #[test]
    fn disabled_cors_enforces_nothing() {
        let mut cfg = config(&["https://app.example.com"], Some(&["GET"]), false);
        cfg.enabled = false;
        assert!(
            check_actual_request(
                Some(&cfg),
                &Method::DELETE,
                &headers(&[("origin", "https://evil.com")])
            )
            .is_ok()
        );
        assert!(check_actual_request(None, &Method::DELETE, &HeaderMap::new()).is_ok());
    }

    #[test]
    fn default_methods_are_get_and_post() {
        let cfg = config(&["https://app.example.com"], None, false);
        let request = headers(&[("origin", "https://app.example.com")]);
        assert!(check_actual_request(Some(&cfg), &Method::GET, &request).is_ok());
        assert!(check_actual_request(Some(&cfg), &Method::POST, &request).is_ok());
        assert!(check_actual_request(Some(&cfg), &Method::PUT, &request).is_err());
    }

    #[test]
    fn wildcard_allows_any_origin() {
        let cfg = config(&["*"], Some(&["GET"]), false);
        assert!(
            check_actual_request(
                Some(&cfg),
                &Method::GET,
                &headers(&[("origin", "https://anything.example")])
            )
            .is_ok()
        );
    }

    #[test]
    fn actual_response_carries_the_echoed_origin_and_extras() {
        let cfg = config(&["https://app.example.com"], Some(&["GET"]), true);
        let request = headers(&[("origin", "https://app.example.com")]);
        let mut out = HeaderMap::new();
        apply_response_headers(&mut out, Some(&cfg), &request);
        assert_eq!(
            out.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(),
            "https://app.example.com"
        );
        assert_eq!(
            out.get(header::ACCESS_CONTROL_EXPOSE_HEADERS).unwrap(),
            "X-Request-ID"
        );
        assert_eq!(
            out.get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS).unwrap(),
            "true"
        );
        assert_eq!(out.get(header::VARY).unwrap(), "Origin");
    }

    #[test]
    fn vary_origin_is_emitted_even_without_cors() {
        let mut out = HeaderMap::new();
        apply_response_headers(&mut out, None, &HeaderMap::new());
        assert_eq!(out.get(header::VARY).unwrap(), "Origin");
        assert!(out.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
    }
}
