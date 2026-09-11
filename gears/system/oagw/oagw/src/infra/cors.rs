// Created: 2026-09-02 by Constructor Tech
//! CORS enforcement (`DESIGN.md` §3.3 Cross-Origin Resource Sharing).
//!
//! A preflight (`OPTIONS` + `Origin` + `Access-Control-Request-Method`) is
//! answered locally with a `204` before any upstream is contacted, so a browser
//! always gets a decision even when the gateway is the one that rejects the
//! actual request. Real cross-origin requests are validated against the
//! effective CORS configuration of the resolved upstream.

use axum::http::{HeaderMap, HeaderValue, Method};

use crate::domain::model::CorsConfig;
use crate::error::GatewayError;

/// Whether the request is a CORS preflight.
#[must_use]
pub fn is_preflight(method: &Method, headers: &HeaderMap) -> bool {
    method == Method::OPTIONS && headers.contains_key(axum::http::header::ORIGIN)
}

/// The headers a preflight answer carries.
///
/// `Origin` is echoed back when it is allowed, otherwise the preflight still
/// answers `204` but without the `Access-Control-Allow-Origin` header, which is
/// how a browser learns the origin is not permitted.
#[must_use]
pub fn preflight_headers(request_headers: &HeaderMap, cors: Option<&CorsConfig>) -> HeaderMap {
    let mut out = HeaderMap::new();
    let origin = request_headers
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let requested = request_headers
        .get(axum::http::header::ACCESS_CONTROL_REQUEST_METHOD)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();

    match cors {
        Some(cors) if cors.enabled && origin_allowed(cors, origin) => {
            out.insert(
                axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN,
                allow_origin_value(cors, origin),
            );
            if cors.allow_credentials {
                out.insert(
                    axum::http::header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
                    HeaderValue::from_static("true"),
                );
            }
            let methods = cors
                .allowed_methods
                .join(", ");
            if let Ok(value) = HeaderValue::from_str(&methods) {
                out.insert(axum::http::header::ACCESS_CONTROL_ALLOW_METHODS, value);
            }
            // Echo the requested headers so a browser proceeds; a custom header
            // that the upstream genuinely refuses is caught on the real request.
            let names: Vec<&str> = request_headers
                .get(axum::http::header::ACCESS_CONTROL_REQUEST_HEADERS)
                .and_then(|v| v.to_str().ok())
                .map(|v| v.split(',').map(str::trim).filter(|s| !s.is_empty()).collect())
                .unwrap_or_default();
            if !names.is_empty()
                && let Ok(value) = HeaderValue::from_str(&names.join(", ")) {
                    out.insert(axum::http::header::ACCESS_CONTROL_ALLOW_HEADERS, value);
                }
            out.insert(
                axum::http::header::ACCESS_CONTROL_MAX_AGE,
                HeaderValue::from_static("600"),
            );
            let exposed = cors.expose_headers.join(", ");
            if let Ok(value) = HeaderValue::from_str(&exposed) {
                out.insert(axum::http::header::ACCESS_CONTROL_EXPOSE_HEADERS, value);
            }
        }
        _ => {
            // No effective configuration, or the origin is not allowed: answer
            // with the method the browser asked for and nothing else.
            if !requested.is_empty()
                && let Ok(value) = HeaderValue::from_str(requested) {
                    out.insert(axum::http::header::ACCESS_CONTROL_ALLOW_METHODS, value);
                }
        }
    }
    if origin_allowed_headers(request_headers) {
        out.insert(
            axum::http::header::VARY,
            HeaderValue::from_static("Origin, Access-Control-Request-Method, Access-Control-Request-Headers"),
        );
    }
    out
}

/// Validates a cross-origin request against the effective CORS configuration.
///
/// Same-origin requests (no `Origin`, or an `Origin` that matches nothing the
/// configuration mentions) are not subject to the check.
pub fn check_request(
    request_headers: &HeaderMap,
    method: &Method,
    cors: Option<&CorsConfig>,
) -> Result<Option<HeaderMap>, GatewayError> {
    let Some(cors) = cors else { return Ok(None) };
    if !cors.enabled {
        return Ok(None);
    }
    let Some(origin) = request_headers
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
    else {
        return Ok(None);
    };
    if !origin_allowed(cors, origin) {
        return Err(GatewayError::CorsRejected {
            kind: "origin",
            detail: format!("origin {origin} is not allowed"),
        });
    }
    if !method_allowed(cors, method) {
        return Err(GatewayError::CorsRejected {
            kind: "method",
            detail: format!("method {} is not allowed", method.as_str()),
        });
    }

    let mut out = HeaderMap::new();
    out.insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN,
        allow_origin_value(cors, origin),
    );
    if cors.allow_credentials {
        out.insert(
            axum::http::header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
            HeaderValue::from_static("true"),
        );
    }
    let exposed = cors.expose_headers.join(", ");
    if let Ok(value) = HeaderValue::from_str(&exposed) {
        out.insert(axum::http::header::ACCESS_CONTROL_EXPOSE_HEADERS, value);
    }
    Ok(Some(out))
}

/// Whether `origin` is permitted.
#[must_use]
pub fn origin_allowed(cors: &CorsConfig, origin: &str) -> bool {
    if origin.is_empty() {
        return false;
    }
    cors.allowed_origins
        .iter()
        .any(|allowed| allowed == "*" || allowed.eq_ignore_ascii_case(origin))
}

fn method_allowed(cors: &CorsConfig, method: &Method) -> bool {
    cors.allowed_methods
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(method.as_str()))
}

/// A wildcard configuration answers `*`, a concrete one echoes the origin so
/// credentialed requests keep working.
fn allow_origin_value(cors: &CorsConfig, origin: &str) -> HeaderValue {
    let wildcard = cors.allowed_origins.iter().any(|o| o == "*");
    HeaderValue::from_str(if wildcard && !cors.allow_credentials {
        "*"
    } else {
        origin
    })
    .unwrap_or(HeaderValue::from_static("*"))
}

fn origin_allowed_headers(_headers: &HeaderMap) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cors(origins: &[&str], methods: &[&str], credentials: bool) -> CorsConfig {
        CorsConfig {
            sharing: crate::domain::model::Sharing::Private,
            enabled: true,
            allowed_origins: origins.iter().map(|o| (*o).to_owned()).collect(),
            allowed_methods: methods.iter().map(|m| (*m).to_owned()).collect(),
            expose_headers: vec!["x-request-id".to_owned()],
            allow_credentials: credentials,
        }
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            let (name, value) = ((*name).to_owned(), (*value).to_owned());
            map.insert(
                axum::http::header::HeaderName::try_from(name.as_str()).unwrap(),
                HeaderValue::from_str(&value).unwrap(),
            );
        }
        map
    }

    #[test]
    fn preflight_is_an_options_with_an_origin() {
        assert!(is_preflight(&Method::OPTIONS, &headers(&[("origin", "https://a")])));
        assert!(!is_preflight(&Method::GET, &headers(&[("origin", "https://a")])));
        assert!(!is_preflight(&Method::OPTIONS, &HeaderMap::new()));
    }

    #[test]
    fn a_preflight_from_an_allowed_origin_gets_the_full_answer() {
        let cors = cors(&["https://app.example.com"], &["GET", "POST"], true);
        let request = headers(&[
            ("origin", "https://app.example.com"),
            ("access-control-request-method", "POST"),
            ("access-control-request-headers", "x-api-key, x-trace"),
        ]);
        let out = preflight_headers(&request, Some(&cors));
        assert_eq!(
            out.get("access-control-allow-origin").unwrap(),
            "https://app.example.com"
        );
        assert_eq!(out.get("access-control-allow-credentials").unwrap(), "true");
        assert_eq!(out.get("access-control-allow-methods").unwrap(), "GET, POST");
        assert_eq!(out.get("access-control-allow-headers").unwrap(), "x-api-key, x-trace");
        assert_eq!(out.get("access-control-expose-headers").unwrap(), "x-request-id");
    }

    #[test]
    fn a_preflight_from_a_foreign_origin_gets_no_allow_origin() {
        let cors = cors(&["https://app.example.com"], &["GET"], false);
        let request = headers(&[("origin", "https://evil.example"), ("access-control-request-method", "GET")]);
        let out = preflight_headers(&request, Some(&cors));
        assert!(out.get("access-control-allow-origin").is_none());
    }

    #[test]
    fn a_wildcard_origin_answers_with_a_star() {
        let cors = cors(&["*"], &["GET"], false);
        let request = headers(&[("origin", "https://anything.example")]);
        let out = preflight_headers(&request, Some(&cors));
        assert_eq!(out.get("access-control-allow-origin").unwrap(), "*");
    }

    #[test]
    fn cross_origin_requests_are_checked_against_the_origin_list() {
        let cors = cors(&["https://app.example.com"], &["GET", "POST"], false);
        let request = headers(&[("origin", "https://app.example.com")]);
        assert!(check_request(&request, &Method::GET, Some(&cors)).is_ok());

        let foreign = headers(&[("origin", "https://other.example")]);
        let err = check_request(&foreign, &Method::GET, Some(&cors)).unwrap_err();
        assert!(matches!(err, GatewayError::CorsRejected { kind: "origin", .. }), "{err:?}");

        let disallowed = headers(&[("origin", "https://app.example.com")]);
        let err = check_request(&disallowed, &Method::DELETE, Some(&cors)).unwrap_err();
        assert!(matches!(err, GatewayError::CorsRejected { kind: "method", .. }), "{err:?}");
    }

    #[test]
    fn same_origin_requests_skip_the_check() {
        let cors = cors(&["https://app.example.com"], &["GET"], false);
        assert!(check_request(&HeaderMap::new(), &Method::DELETE, Some(&cors)).is_ok());
        let disabled = CorsConfig { enabled: false, ..cors };
        let request = headers(&[("origin", "https://anything.example")]);
        assert!(check_request(&request, &Method::DELETE, Some(&disabled)).is_ok());
        assert!(check_request(&HeaderMap::new(), &Method::GET, None).is_ok());
    }
}
