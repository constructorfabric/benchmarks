//! CORS on the proxy path (ADR-0004).
//!
//! ADR-0004 rejects both "proxy CORS to the upstream" and "CORS as a guard
//! plugin" in favour of a *built-in* handler, and it puts the two halves of the
//! protocol in two different places:
//!
//! * the **preflight** is answered at the handler level, before anything is
//!   resolved — a browser preflight carries no credentials (WHATWG Fetch spec),
//!   so there is no tenant context to resolve an upstream with, and the answer
//!   must not depend on a round trip to a possibly unavailable upstream. It is
//!   *permissive*: it echoes what the browser asked for and defers every
//!   decision. A browser that was lied to finds out on the actual request,
//!   which is rejected with a 403 problem document before the upstream is
//!   contacted.
//! * the **actual request** is validated after the upstream is resolved — that
//!   is where the `cors` document of the upstream or route is known — and
//!   before the request is forwarded.
//!
//! The module documentation of [`crate::domain::proxy`] records where each half
//! runs relative to the plugin chain.

use http::header::{
    ACCESS_CONTROL_ALLOW_CREDENTIALS, ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS,
    ACCESS_CONTROL_ALLOW_ORIGIN, ACCESS_CONTROL_EXPOSE_HEADERS, ACCESS_CONTROL_MAX_AGE,
    ACCESS_CONTROL_REQUEST_HEADERS, ACCESS_CONTROL_REQUEST_METHOD, ORIGIN, VARY,
};
use http::{HeaderMap, HeaderValue, Method};

use crate::domain::model::CorsConfig;
use crate::error::OagwError;

/// `Access-Control-Max-Age` of a preflight answer (ADR-0004): a browser may
/// cache the answer for a day.
pub const PREFLIGHT_MAX_AGE: &str = "86400";

/// The `Vary` value a preflight answer carries: every header the answer was
/// computed from, so no shared cache may reuse it for another request.
const PREFLIGHT_VARY: &str =
    "Origin, Access-Control-Request-Method, Access-Control-Request-Headers";

/// Whether the request is a CORS preflight: `OPTIONS` plus the two headers a
/// browser adds to ask for permission (ADR-0004 "Preflight Request Handling").
#[must_use]
pub fn is_preflight(method: &Method, headers: &HeaderMap) -> bool {
    method == Method::OPTIONS
        && headers.contains_key(ORIGIN)
        && headers.contains_key(ACCESS_CONTROL_REQUEST_METHOD)
}

/// The 204 a preflight is answered with (ADR-0004): the requested origin,
/// method and headers are echoed verbatim, with the caching advice that keeps a
/// shared cache from reusing the answer for a different request.
///
/// The answer is deliberately independent of any `cors` document: it is
/// produced before the upstream is resolved and therefore before the policy of
/// the target is known. Origin and method enforcement happens on the actual
/// request that follows.
#[must_use]
pub fn preflight_response(request_headers: &HeaderMap) -> axum::response::Response {
    use axum::body::Body;
    use axum::http::StatusCode;
    use http::Response;

    let echo = |name| request_headers.get(name).cloned();
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::NO_CONTENT;
    let headers = response.headers_mut();
    if let Some(origin) = echo(ORIGIN) {
        headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    }
    if let Some(method) = echo(ACCESS_CONTROL_REQUEST_METHOD) {
        headers.insert(ACCESS_CONTROL_ALLOW_METHODS, method);
    }
    if let Some(requested) = echo(ACCESS_CONTROL_REQUEST_HEADERS) {
        headers.insert(ACCESS_CONTROL_ALLOW_HEADERS, requested);
    }
    if let Ok(max_age) = HeaderValue::from_str(PREFLIGHT_MAX_AGE) {
        headers.insert(ACCESS_CONTROL_MAX_AGE, max_age);
    }
    headers.insert(VARY, HeaderValue::from_static(PREFLIGHT_VARY));
    response
}

/// Enforce the CORS rules of a resolved resource on an actual request
/// (ADR-0004 "Actual Request Handling"): the `Origin` first, then the method.
///
/// # Errors
///
/// Returns the 403 [`OagwError`] of the first rule the request violates —
/// `cors.origin_not_allowed.v1` for a foreign origin, `cors.method_not_allowed.v1`
/// for a method the resource does not advertise.
pub fn enforce(
    config: &CorsConfig,
    method: &Method,
    request_headers: &HeaderMap,
) -> Result<Option<HeaderValue>, OagwError> {
    if !config.enabled {
        return Ok(None);
    }
    let Some(origin) = request_headers.get(ORIGIN) else {
        // No `Origin`, so this is not a cross-origin browser request and CORS
        // says nothing about it (PRD §5.3: CORS only affects browser clients).
        return Ok(None);
    };
    let requested = origin.to_str().unwrap_or_default();
    if !origin_allowed(config, requested) {
        return Err(OagwError::cors_origin_not_allowed(format!(
            "Origin '{requested}' not in allowed origins list"
        ))
        .with_invalid_value(requested.to_owned()));
    }
    if !method_allowed(config, method) {
        return Err(OagwError::cors_method_not_allowed(format!(
            "Method '{}' not in allowed methods list",
            method.as_str()
        ))
        .with_invalid_value(method.as_str().to_owned()));
    }
    Ok(Some(origin.clone()))
}

/// Whether the resource admits `origin` (ADR-0004 "Origin Matching"): an exact,
/// port- and protocol-sensitive match, or the wildcard. There is deliberately no
/// pattern matching, so an origin can only be admitted by being spelled out.
fn origin_allowed(config: &CorsConfig, origin: &str) -> bool {
    config.allowed_origins.as_ref().is_some_and(|origins| {
        origins
            .iter()
            .any(|allowed| allowed.trim() == "*" || allowed.trim() == origin)
    })
}

/// Whether the resource admits `method` (ADR-0004 "Actual Request Handling").
fn method_allowed(config: &CorsConfig, method: &Method) -> bool {
    config
        .allowed_methods
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(method.as_str()))
}

/// Add the CORS headers of an admitted response (ADR-0004 "Actual Request
/// Handling"): the allowed origin, the credentials rule and the headers a
/// browser may read, plus `Vary: Origin` — which is *appended*, so a `Vary` the
/// upstream set for its own reasons keeps describing the response.
pub fn apply_response_headers(config: &CorsConfig, origin: &HeaderValue, response: &mut HeaderMap) {
    let wildcard = config
        .allowed_origins
        .as_ref()
        .is_some_and(|origins| origins.iter().any(|allowed| allowed.trim() == "*"));
    let advertised = if wildcard {
        HeaderValue::from_static("*")
    } else {
        origin.clone()
    };
    response.insert(ACCESS_CONTROL_ALLOW_ORIGIN, advertised);
    if config.allow_credentials && !wildcard {
        response.insert(
            ACCESS_CONTROL_ALLOW_CREDENTIALS,
            HeaderValue::from_static("true"),
        );
    }
    if let Ok(exposed) = HeaderValue::from_str(&config.expose_headers.join(", "))
        && !config.expose_headers.is_empty()
    {
        response.insert(ACCESS_CONTROL_EXPOSE_HEADERS, exposed);
    }
    response.append(VARY, HeaderValue::from_static("Origin"));
}

#[cfg(test)]
#[path = "cors_tests.rs"]
mod tests;
