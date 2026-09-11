//! CORS enforcement (ADR-0004).
//!
//! CORS is a first-class field on an upstream, not a plugin, and the two sides of the
//! protocol are handled at different points:
//!
//! - a **preflight** `OPTIONS` is answered in the proxy handler, permissively and
//!   *before* the caller is resolved — a browser preflight carries no credentials, so
//!   there is no tenant context to resolve an upstream with. The requested origin,
//!   method and headers are echoed back; whether the caller is actually allowed is
//!   decided on the request that follows.
//! - an **actual** cross-origin request is validated against the origin and method
//!   allowlists of the resolved upstream before it is forwarded, and annotated with the
//!   CORS response headers when it survives.

use axum::http::Method;
use http::HeaderMap;

use crate::domain::upstream::CorsConfig;

/// `Vary` lists every request header the preflight answer depends on.
pub const PREFLIGHT_VARY: &str = "Origin, Access-Control-Request-Method, Access-Control-Request-Headers";

/// Preflight answers may be cached for a day.
pub const PREFLIGHT_MAX_AGE_SECS: u32 = 86_400;

/// Whether this request is a CORS preflight.
#[must_use]
pub fn is_preflight(method: &Method, headers: &HeaderMap) -> bool {
    method == Method::OPTIONS
        && headers.contains_key(http::header::ORIGIN)
        && headers.contains_key(http::header::ACCESS_CONTROL_REQUEST_METHOD)
}

/// Builds a preflight response.
///
/// The answer is permissive by design (ADR-0004): the browser's preflight carries no
/// credentials, so there is no tenant context to resolve an upstream with, and the
/// allowlists are enforced on the *actual* request instead. Echoing the request back
/// here keeps the browser's failure legible — it surfaces on the real request as a
/// `403`, not as an opaque preflight failure.
#[must_use]
pub fn preflight_response(headers: &HeaderMap) -> axum::response::Response {
    // Built directly rather than through a builder: nothing here can fail, so the
    // builder's `Result` would be noise.
    let mut response = axum::http::Response::new(axum::body::Body::empty());
    *response.status_mut() = axum::http::StatusCode::NO_CONTENT;

    // Without an `Origin` this is not a CORS request, so there is nothing to echo: the
    // answer stays a bare `204`.
    let Some(origin) = headers.get(http::header::ORIGIN) else {
        return response;
    };

    let out = response.headers_mut();
    out.append(http::header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
    // What the caller *asked for* is what the answer *allows*: a preflight echoes its
    // own request back, never the allowlist of an upstream that has not been resolved.
    for value in headers.get_all(http::header::ACCESS_CONTROL_REQUEST_METHOD) {
        out.append(http::header::ACCESS_CONTROL_ALLOW_METHODS, value.clone());
    }
    for value in headers.get_all(http::header::ACCESS_CONTROL_REQUEST_HEADERS) {
        out.append(http::header::ACCESS_CONTROL_ALLOW_HEADERS, value.clone());
    }
    insert(out, http::header::ACCESS_CONTROL_MAX_AGE, &PREFLIGHT_MAX_AGE_SECS.to_string());
    insert(out, http::header::VARY, PREFLIGHT_VARY);
    response
}

/// Applies the CORS response headers to an actual cross-origin response.
///
/// Nothing is added when the origin is not allowed — the request was already rejected.
pub fn apply_to_response(
    config: &CorsConfig,
    origin: &str,
    method: &Method,
    response: &mut axum::response::Response,
) {
    let Some(reflected) = reflected_origin(config, origin) else {
        return;
    };
    let headers = response.headers_mut();
    if headers.get(http::header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none() {
        headers.insert(http::header::ACCESS_CONTROL_ALLOW_ORIGIN, reflected);
    }
    if config.allow_credentials {
        insert(headers, http::header::ACCESS_CONTROL_ALLOW_CREDENTIALS, "true");
    }
    if !config.expose_headers.is_empty() {
        insert(
            headers,
            http::header::ACCESS_CONTROL_EXPOSE_HEADERS,
            &config.expose_headers.join(", "),
        );
    }
    append(headers, http::header::VARY, "Origin");
    let _ = method;
}

/// Whether `origin` is allowed by the configuration.
///
/// Matching is exact and case-insensitive: ports and schemes are significant, and there
/// are no wildcard patterns except the single `*` entry, which means any origin. A
/// wildcard cannot be combined with credentials — that combination is rejected at
/// configuration time and, if it somehow reaches here, denies the request.
#[must_use]
pub fn origin_allowed(config: &CorsConfig, origin: &str) -> bool {
    if !config.enabled {
        return false;
    }
    config.allowed_origins.iter().any(|allowed| {
        if allowed == "*" {
            !config.allow_credentials
        } else {
            allowed.eq_ignore_ascii_case(origin)
        }
    })
}

/// Whether a cross-origin method is allowed.
#[must_use]
pub fn method_allowed(config: &CorsConfig, method: &Method) -> bool {
    config
        .allowed_methods
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(method.as_str()))
}

/// The origin to reflect in `Access-Control-Allow-Origin`, if any.
///
/// The wildcard is reflected literally only when credentials are not involved; a
/// credentialed response must name the origin exactly.
#[must_use]
fn reflected_origin(config: &CorsConfig, origin: &str) -> Option<http::HeaderValue> {
    if !config.enabled {
        return None;
    }
    for allowed in &config.allowed_origins {
        if allowed.eq_ignore_ascii_case(origin) {
            return http::HeaderValue::from_str(origin).ok();
        }
    }
    if config.allowed_origins.iter().any(|a| a == "*") && !config.allow_credentials {
        return Some(http::HeaderValue::from_static("*"));
    }
    None
}

fn insert(out: &mut http::HeaderMap, name: http::HeaderName, value: &str) {
    if let Ok(value) = http::HeaderValue::from_str(value) {
        out.insert(name, value);
    }
}

fn append(out: &mut http::HeaderMap, name: http::HeaderName, value: &str) {
    if let Ok(value) = http::HeaderValue::from_str(value) {
        out.append(name, value);
    }
}
