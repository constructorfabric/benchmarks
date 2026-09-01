//! CORS handling (ADR-0004).
//!
//! * Preflight (`OPTIONS` + `Origin` + `Access-Control-Request-Method`) is
//!   answered at the handler level with a permissive `204` that echoes the
//!   requested origin/method/headers — no upstream resolution, no auth.
//! * Actual (non-preflight) requests are validated against the effective
//!   `CorsConfig` after upstream resolution: disallowed origin or method
//!   yields `403` with the dedicated GTS identifiers.

use http::{HeaderMap, Method, Request, Response, StatusCode};

use crate::domain::error::ProxyError;
use crate::domain::model::CorsConfig;

/// Whether this is a CORS preflight request.
#[must_use]
pub fn is_preflight(req: &Request<()>) -> bool {
    req.method() == Method::OPTIONS
        && req.headers().contains_key(http::header::ORIGIN)
        && req.headers().contains_key("access-control-request-method")
}

/// Build the permissive preflight response (RFC 204 echo).
#[must_use]
pub fn preflight_response(req: &Request<()>) -> Response<axum::body::Body> {
    let headers = req.headers();
    let mut resp = Response::new(axum::body::Body::empty());
    *resp.status_mut() = StatusCode::NO_CONTENT;
    let out = resp.headers_mut();
    if let Some(origin) = headers.get(http::header::ORIGIN) {
        out.insert(http::header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
    }
    if let Some(method) = headers.get("access-control-request-method") {
        out.insert(http::header::ACCESS_CONTROL_ALLOW_METHODS, method.clone());
    }
    if let Some(requested) = headers.get("access-control-request-headers") {
        out.insert(
            http::header::ACCESS_CONTROL_ALLOW_HEADERS,
            requested.clone(),
        );
    }
    out.insert(
        http::header::ACCESS_CONTROL_MAX_AGE,
        http::HeaderValue::from_static("86400"),
    );
    out.insert(
        http::header::VARY,
        http::HeaderValue::from_static(
            "Origin, Access-Control-Request-Method, Access-Control-Request-Headers",
        ),
    );
    resp
}

/// Validate an actual (non-preflight) request against the effective CORS
/// config.
///
/// # Errors
///
/// Returns [`ProxyError::CorsOriginNotAllowed`] or
/// [`ProxyError::CorsMethodNotAllowed`].
pub fn check_actual(cors: &CorsConfig, req: &Request<()>) -> Result<(), ProxyError> {
    if !cors.enabled {
        return Ok(());
    }
    let origin = req.headers().get(http::header::ORIGIN);
    let Some(origin) = origin.and_then(|v| v.to_str().ok()) else {
        return Ok(());
    };
    let wildcard = cors.allowed_origins.iter().any(|o| o == "*");
    let allowed = wildcard || cors.allowed_origins.iter().any(|o| o == origin);
    if !allowed {
        return Err(ProxyError::CorsOriginNotAllowed {
            context: crate::domain::error::ProxyContext::default(),
            origin: origin.to_owned(),
        });
    }
    let method = req.method().as_str();
    let method_ok = cors
        .allowed_methods
        .iter()
        .any(|m| m.eq_ignore_ascii_case(method));
    if !method_ok {
        return Err(ProxyError::CorsMethodNotAllowed {
            context: crate::domain::error::ProxyContext::default(),
            method: method.to_owned(),
        });
    }
    Ok(())
}

/// Attach CORS response headers for an actual (successful) request.
pub fn enrich_response(cors: &CorsConfig, req: &Request<()>, headers: &mut HeaderMap) {
    if !cors.enabled {
        return;
    }
    let Some(origin) = req
        .headers()
        .get(http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
    else {
        return;
    };
    let wildcard = cors.allowed_origins.iter().any(|o| o == "*");
    let value = if wildcard {
        http::HeaderValue::from_static("*")
    } else {
        http::HeaderValue::from_str(origin).unwrap_or_else(|_| http::HeaderValue::from_static("*"))
    };
    headers.insert(http::header::ACCESS_CONTROL_ALLOW_ORIGIN, value);
    if !cors.expose_headers.is_empty() {
        let joined = cors.expose_headers.join(", ");
        if let Ok(v) = http::HeaderValue::from_str(&joined) {
            headers.insert(http::header::ACCESS_CONTROL_EXPOSE_HEADERS, v);
        }
    }
    if cors.allow_credentials {
        headers.insert(
            http::header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
            http::HeaderValue::from_static("true"),
        );
    }
    headers.insert(http::header::VARY, http::HeaderValue::from_static("Origin"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{CorsConfig, SharingMode};

    fn req_with_origin(method: &'static str, origin: Option<&str>) -> Request<()> {
        let mut builder = Request::builder().method(method).uri("/proxy/x/p");
        if let Some(origin) = origin {
            builder = builder.header(http::header::ORIGIN, origin);
        }
        builder.body(()).expect("valid request")
    }

    #[test]
    fn detects_preflight() {
        let mut req = Request::builder()
            .method(Method::OPTIONS)
            .uri("/proxy/x/p")
            .header(http::header::ORIGIN, "https://app.example.com")
            .header("access-control-request-method", "POST")
            .body(())
            .expect("valid");
        assert!(is_preflight(&req));
        req.headers_mut().remove("access-control-request-method");
        assert!(!is_preflight(&req));
    }

    #[test]
    fn preflight_echoes_requested_headers() {
        let req = Request::builder()
            .method(Method::OPTIONS)
            .uri("/proxy/x/p")
            .header(http::header::ORIGIN, "https://app.example.com")
            .header("access-control-request-method", "POST")
            .header("access-control-request-headers", "Content-Type")
            .body(())
            .expect("valid");
        let resp = preflight_response(&req);
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let headers = resp.headers();
        assert_eq!(
            headers
                .get(http::header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|v| v.to_str().ok()),
            Some("https://app.example.com")
        );
        assert_eq!(
            headers
                .get(http::header::ACCESS_CONTROL_MAX_AGE)
                .and_then(|v| v.to_str().ok()),
            Some("86400")
        );
    }

    #[test]
    fn actual_request_validates_origin_and_method() {
        let cors = CorsConfig {
            sharing: SharingMode::default(),
            enabled: true,
            allowed_origins: vec!["https://app.example.com".to_owned()],
            allowed_methods: vec!["GET".to_owned(), "POST".to_owned()],
            expose_headers: Vec::new(),
            allow_credentials: false,
        };
        let ok = req_with_origin("POST", Some("https://app.example.com"));
        assert!(check_actual(&cors, &ok).is_ok());

        let bad_origin = req_with_origin("POST", Some("https://evil.example.com"));
        assert!(matches!(
            check_actual(&cors, &bad_origin),
            Err(ProxyError::CorsOriginNotAllowed { .. })
        ));

        let bad_method = req_with_origin("DELETE", Some("https://app.example.com"));
        assert!(matches!(
            check_actual(&cors, &bad_method),
            Err(ProxyError::CorsMethodNotAllowed { .. })
        ));
    }
}
