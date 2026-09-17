//! CORS handling (ADR-0004).
//!
//! * Preflight (`OPTIONS` + `Origin` + `Access-Control-Request-Method`) is
//!   answered at the handler level with a permissive `204` that echoes the
//!   requested method/headers — no upstream resolution, no tenant context,
//!   no plugin execution.
//! * Actual requests are checked after upstream resolution: origin (exact
//!   match, port/protocol sensitive; `*` matches any) then method. Passthrough
//!   responses gain `Access-Control-Allow-Origin` (+ `Expose-Headers`,
//!   `Allow-Credentials`, `Vary: Origin`) only when the request carried an
//!   `Origin` header.

use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};

use crate::domain::model::CorsConfig;
use crate::error::OagwError;

/// Build a header value, defaulting to an empty value when invalid.
fn hv(s: &str) -> HeaderValue {
    HeaderValue::from_str(s).unwrap_or_else(|_| HeaderValue::from_static(""))
}

/// Whether this request is a CORS preflight.
#[must_use]
pub fn is_cors_preflight(method: &Method, headers: &HeaderMap) -> bool {
    method == Method::OPTIONS
        && headers.contains_key("origin")
        && headers.contains_key("access-control-request-method")
}

/// Build the permissive preflight response (echo 204 per ADR-0004).
#[must_use]
pub fn build_preflight_response(headers: &HeaderMap) -> axum::response::Response {
    let origin = headers
        .get("origin")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("*")
        .to_owned();
    let method = headers
        .get("access-control-request-method")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("GET")
        .to_owned();
    let requested_headers = headers
        .get("access-control-request-headers")
        .and_then(|v| v.to_str().ok());

    let mut resp = axum::response::Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header("access-control-allow-origin", origin)
        .header("access-control-allow-methods", method)
        .header("access-control-max-age", "86400")
        .header(
            "vary",
            "Origin, Access-Control-Request-Method, Access-Control-Request-Headers",
        )
        .body(axum::body::Body::empty())
        .expect("preflight response is statically valid");
    if let Some(req_headers) = requested_headers {
        resp.headers_mut()
            .insert("access-control-allow-headers", hv(req_headers));
    }
    resp
}

/// Check the request `Origin` against the CORS config. Only called when the
/// request actually carries an Origin header and CORS is enabled.
pub fn check_origin(cors: &CorsConfig, origin: &str) -> Result<(), OagwError> {
    let allowed = cors.allowed_origins.iter().any(|o| {
        o == "*" || o.eq_ignore_ascii_case(origin)
    });
    if allowed {
        Ok(())
    } else {
        Err(OagwError::CorsOriginNotAllowed {
            detail: format!(
                "origin `{origin}` is not in the allowed origins for this upstream"
            ),
        })
    }
}

/// Check the request method against the CORS config.
pub fn check_method(cors: &CorsConfig, method: &Method) -> Result<(), OagwError> {
    let allowed = cors.allowed_methods.iter().any(|m| m.eq_ignore_ascii_case(method.as_str()));
    if allowed {
        Ok(())
    } else {
        Err(OagwError::CorsMethodNotAllowed {
            detail: format!(
                "method `{}` is not in the allowed methods for this upstream",
                method.as_str()
            ),
        })
    }
}

/// Apply CORS response headers to a proxied response (actual request only).
pub fn apply_response_headers(
    cors: &CorsConfig,
    origin: Option<&str>,
    response_headers: &mut HeaderMap,
) {
    match origin {
        Some(origin) if !cors.allowed_origins.iter().any(|o| o == "*") => {
            response_headers.insert(
                "access-control-allow-origin",
                hv(origin),
            );
        }
        Some(_) => {
            // Wildcard config: still echo the concrete origin so browsers
            // accept it with credentials.
            if cors.allow_credentials {
                if let Some(origin) = origin {
                    response_headers.insert(
                        "access-control-allow-origin",
                        hv(origin),
                    );
                }
            } else {
                response_headers.insert(
                    "access-control-allow-origin",
                    HeaderValue::from_static("*"),
                );
            }
        }
        None => {}
    }
    if !cors.expose_headers.is_empty() {
        response_headers.insert(
            "access-control-expose-headers",
            hv(&cors.expose_headers.join(", ")),
        );
    }
    if cors.allow_credentials {
        response_headers.insert(
            "access-control-allow-credentials",
            HeaderValue::from_static("true"),
        );
    }
    response_headers.insert("vary", HeaderValue::from_static("Origin"));
}
