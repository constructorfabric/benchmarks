//! CORS enforcement for the data plane (ADR 0004).
//!
//! - Preflight (`OPTIONS`) requests are answered with a permissive `204`
//!   (origin, methods, headers echoed) so browsers may proceed.
//! - Actual requests whose `Origin` is not allowed are rejected with
//!   `403 cors.origin_not_allowed`; HTTP methods outside `allowed_methods`
//!   are rejected with `403 cors.method_not_allowed`.
//! - `allow_credentials: true` combined with a `*` origin is invalid by
//!   schema validation (400 `invalid_request` on write) and here.
//! - Responses carry `Access-Control-Allow-Origin` + `Vary: Origin`.

use http::header::{HeaderValue, CONTENT_TYPE, ORIGIN, VARY};
use http::HeaderMap;
use http::Method;

use crate::domain::plugin::ProxyResponseView;
use crate::domain::gts as g;
use crate::domain::model::CorsConfig;

/// Outcome of a CORS check on an actual (non-preflight) request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CorsDecision {
    /// No CORS applies (CORS disabled, non-browser request, or allowed).
    Proceed,
    /// Request rejected with the given status + GTS type.
    Rejected { status: u16, gts_type: &'static str, detail: String },
}

fn origin_of(headers: &HeaderMap) -> Option<&str> {
    headers.get(ORIGIN).and_then(|v| v.to_str().ok())
}

/// Handle a `OPTIONS *` preflight. Returns the response to send back.
#[must_use]
pub fn preflight_response(request_headers: &HeaderMap) -> http::Response<()> {
    let mut response = http::Response::builder()
        .status(http::StatusCode::NO_CONTENT)
        .body(())
        .expect("static preflight response");
    let origin = origin_of(request_headers);
    let allow_origin = origin.unwrap_or("*");
    let headers = response.headers_mut();
    headers.insert(
        "access-control-allow-origin",
        HeaderValue::from_str(allow_origin).expect("header value"),
    );
    headers.insert(
        "access-control-allow-methods",
        HeaderValue::from_static("GET, POST, PUT, PATCH, DELETE, OPTIONS, HEAD"),
    );
    headers.insert(
        "access-control-allow-headers",
        HeaderValue::from_static("authorization, content-type, x-request-id, x-oagw-target-host, x-api-key"),
    );
    headers.insert(
        "access-control-max-age",
        HeaderValue::from_static("600"),
    );
    headers.insert(
        VARY,
        HeaderValue::from_static("Origin"),
    );
    response
}

/// Validate an actual request's origin/method against the effective CORS
/// config. `allowed_methods` on the config are matched case-insensitively.
///
/// # Errors
///
/// When CORS is enabled and the request carries an `Origin`, a disallowed
/// origin or method yields [`CorsDecision::Rejected`].
#[must_use]
pub fn check_actual(config: &CorsConfig, method: &Method, headers: &HeaderMap) -> CorsDecision {
    if !config.enabled {
        return CorsDecision::Proceed;
    }
    let Some(origin) = origin_of(headers) else {
        // Not a browser CORS request — no enforcement.
        return CorsDecision::Proceed;
    };

    let allowed = |candidate: &str, item: &str| {
        let a = item.to_ascii_lowercase();
        let b = candidate.to_ascii_lowercase();
        if a == "*" {
            !config.allow_credentials
        } else {
            a == b
        }
    };

    let origin_ok = config.allowed_origins.is_empty()
        || config.allowed_origins.iter().any(|o| allowed(origin, o));
    if !origin_ok {
        return CorsDecision::Rejected {
            status: 403,
            gts_type: g::ERR_CORS_ORIGIN_NOT_ALLOWED,
            detail: format!("origin '{origin}' is not allowed by CORS policy"),
        };
    }

    let method_name = method.as_str();
    let method_ok = if config.allowed_methods.is_empty() {
        true
    } else {
        config.allowed_methods.iter().any(|m| allowed(method_name, m))
    };
    if !method_ok {
        return CorsDecision::Rejected {
            status: 403,
            gts_type: g::ERR_CORS_METHOD_NOT_ALLOWED,
            detail: format!("method '{method_name}' is not allowed by CORS policy"),
        };
    }

    CorsDecision::Proceed
}

/// Apply CORS response headers to a proxied response (actual request).
pub fn apply_response_headers(
    config: &CorsConfig,
    request_headers: &HeaderMap,
    view: &mut ProxyResponseView,
) {
    let Some(origin) = origin_of(request_headers) else {
        return;
    };
    let allow_origin = if config.allowed_origins.iter().any(|o| o == "*") && !config.allow_credentials {
        "*"
    } else {
        origin
    };
    // Find-or-insert: hyper's HeaderMap entry API.
    view.headers
        .entry("access-control-allow-origin")
        .or_insert_with(|| HeaderValue::from_str(allow_origin).expect("origin header value"));
    if config.allow_credentials {
        view.headers
            .entry("access-control-allow-credentials")
            .or_insert_with(|| HeaderValue::from_static("true"));
    }
    if !config.expose_headers.is_empty() {
        let joined = config.expose_headers.join(", ");
        view.headers
            .entry("access-control-expose-headers")
            .or_insert_with(|| HeaderValue::from_str(&joined).expect("expose header value"));
    }
    // Vary: Origin — append if absent.
    let add_vary = view.headers.get(VARY).map_or(true, |v| {
        !v.to_str().map_or(false, |s| s.to_ascii_lowercase().contains("origin"))
    });
    if add_vary {
        view.headers.insert(VARY, HeaderValue::from_static("Origin"));
    }
    let _ = CONTENT_TYPE; // keep import symmetry with HTTP semantics
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_with_origin(origin: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(ORIGIN, HeaderValue::from_str(origin).unwrap());
        h
    }

    #[test]
    fn disabled_cors_proceeds_even_with_origin() {
        let cfg = CorsConfig::default(); // enabled: false
        assert_eq!(
            check_actual(&cfg, &Method::GET, &headers_with_origin("https://evil.example")),
            CorsDecision::Proceed
        );
    }

    #[test]
    fn disallowed_origin_rejected_403() {
        let cfg = CorsConfig {
            enabled: true,
            allowed_origins: vec!["https://good.example".into()],
            ..CorsConfig::default()
        };
        assert_eq!(
            check_actual(&cfg, &Method::GET, &headers_with_origin("https://evil.example")),
            CorsDecision::Rejected {
                status: 403,
                gts_type: g::ERR_CORS_ORIGIN_NOT_ALLOWED,
                detail: "origin 'https://evil.example' is not allowed by CORS policy"
                    .to_owned(),
            }
        );
    }

    #[test]
    fn disallowed_method_rejected_403() {
        let cfg = CorsConfig {
            enabled: true,
            allowed_origins: vec!["https://good.example".into()],
            allowed_methods: vec!["GET".into(), "POST".into()],
            ..CorsConfig::default()
        };
        let h = headers_with_origin("https://good.example");
        // A DELETE request must fail on method.
        assert_eq!(
            check_actual(&cfg, &Method::DELETE, &h),
            CorsDecision::Rejected {
                status: 403,
                gts_type: g::ERR_CORS_METHOD_NOT_ALLOWED,
                detail: "method 'DELETE' is not allowed by CORS policy".to_owned(),
            }
        );
        assert_eq!(
            check_actual(&cfg, &Method::GET, &h),
            CorsDecision::Proceed
        );
    }

    #[test]
    fn wildcard_origin_without_credentials_allowed() {
        let cfg = CorsConfig {
            enabled: true,
            allowed_origins: vec!["*".into()],
            allow_credentials: false,
            ..CorsConfig::default()
        };
        assert_eq!(
            check_actual(&cfg, &Method::GET, &headers_with_origin("https://any.example")),
            CorsDecision::Proceed
        );
    }

    #[test]
    fn preflight_is_204_with_vary_origin() {
        let mut headers = HeaderMap::new();
        headers.insert(ORIGIN, HeaderValue::from_static("https://a.example"));
        let resp = preflight_response(&headers);
        assert_eq!(resp.status(), http::StatusCode::NO_CONTENT);
        assert_eq!(
            resp.headers().get("access-control-allow-origin").unwrap(),
            "https://a.example"
        );
        assert_eq!(resp.headers().get(VARY).unwrap(), "Origin");
    }

    fn resp_view() -> ProxyResponseView {
        ProxyResponseView {
            status: http::StatusCode::OK,
            headers: HeaderMap::new(),
            body_len: 0,
        }
    }

    #[test]
    fn apply_response_headers_echoes_origin() {
        let cfg = CorsConfig {
            enabled: true,
            allowed_origins: vec!["https://good.example".into()],
            allow_credentials: true,
            expose_headers: vec!["x-total".into()],
            ..CorsConfig::default()
        };
        let req = headers_with_origin("https://good.example");
        let mut view = resp_view();
        apply_response_headers(&cfg, &req, &mut view);
        assert_eq!(
            view.headers.get("access-control-allow-origin").unwrap(),
            "https://good.example"
        );
        assert_eq!(
            view.headers.get("access-control-allow-credentials").unwrap(),
            "true"
        );
        assert_eq!(view.headers.get("access-control-expose-headers").unwrap(), "x-total");
        assert_eq!(view.headers.get(VARY).unwrap(), "Origin");
    }

    #[test]
    fn apply_response_headers_wildcard_without_credentials_uses_star() {
        let cfg = CorsConfig {
            enabled: true,
            allowed_origins: vec!["*".into()],
            allow_credentials: false,
            ..CorsConfig::default()
        };
        let req = headers_with_origin("https://any.example");
        let mut view = resp_view();
        apply_response_headers(&cfg, &req, &mut view);
        assert_eq!(view.headers.get("access-control-allow-origin").unwrap(), "*");
        assert!(view.headers.get("access-control-allow-credentials").is_none());
    }

    #[test]
    fn apply_response_headers_noop_without_origin() {
        let cfg = CorsConfig {
            enabled: true,
            allowed_origins: vec!["https://good.example".into()],
            ..CorsConfig::default()
        };
        let mut view = resp_view();
        apply_response_headers(&cfg, &HeaderMap::new(), &mut view);
        assert!(view.headers.is_empty());
    }

    #[test]
    fn apply_response_headers_does_not_duplicate_vary() {
        let cfg = CorsConfig {
            enabled: true,
            allowed_origins: vec!["https://good.example".into()],
            ..CorsConfig::default()
        };
        let req = headers_with_origin("https://good.example");
        let mut view = resp_view();
        view.headers.insert(VARY, HeaderValue::from_static("Origin, Accept"));
        apply_response_headers(&cfg, &req, &mut view);
        assert_eq!(view.headers.get(VARY).unwrap(), "Origin, Accept");
    }
}
