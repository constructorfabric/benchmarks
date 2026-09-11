//! CORS handling (ADR 0004).
//!
//! Preflights are answered locally at the handler level with a permissive `204`
//! that echoes the requested method and headers — before the upstream is
//! resolved, before a tenant context is required and without any upstream call.
//! Actual cross-origin requests are validated against the upstream's CORS
//! configuration.

use http::{HeaderMap, Method, StatusCode};

use crate::domain::error::DomainError;
use crate::domain::gts_helpers as gts;
use crate::domain::model::CorsConfig;

/// The preflight request headers the gateway acts on.
pub const PREFLIGHT_METHOD_HEADER: &str = "access-control-request-method";
/// The preflight header naming request headers.
pub const PREFLIGHT_HEADERS_HEADER: &str = "access-control-request-headers";

/// Whether this request is a CORS preflight (`OPTIONS` + `Origin` +
/// `Access-Control-Request-Method`).
#[must_use]
pub fn is_preflight(method: &Method, headers: &HeaderMap) -> bool {
    method == Method::OPTIONS
        && headers.contains_key(http::header::ORIGIN)
        && headers.contains_key(PREFLIGHT_METHOD_HEADER)
}

/// Builds the preflight `204` response, echoing the request's origin, the
/// method it asks about and the headers it wants to send (ADR 0004).
///
/// Browsers enforce this echo themselves: a preflight answer that does not
/// name the origin and method of the pending request is treated as a failure,
/// so `*` — while permissive — would not let an actual cross-origin request
/// through. No upstream is resolved and no tenant context is required.
#[must_use]
pub fn preflight_response(headers: &HeaderMap) -> http::Response<axum::body::Body> {
    let echo = |name: &str, fallback: &'static str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map_or_else(|| fallback.to_owned(), str::to_owned)
    };
    let origin = echo(http::header::ORIGIN.as_str(), "*");
    let methods = echo(PREFLIGHT_METHOD_HEADER, "*");
    let requested_headers = echo(PREFLIGHT_HEADERS_HEADER, "*");
    // Built by hand rather than through the fallible builder: well-known
    // header names plus request-echoed values cannot fail to construct.
    let mut response = http::Response::new(axum::body::Body::empty());
    *response.status_mut() = StatusCode::NO_CONTENT;
    let out = response.headers_mut();
    let insert = |out: &mut http::HeaderMap, name: &'static str, value: String| {
        if let Ok(value) = http::HeaderValue::from_str(&value) {
            out.insert(http::HeaderName::from_static(name), value);
        }
    };
    insert(out, "access-control-allow-origin", origin);
    insert(out, "access-control-allow-methods", methods);
    insert(out, "access-control-allow-headers", requested_headers);
    insert(out, "access-control-max-age", "86400".to_owned());
    insert(
        out,
        http::header::VARY.as_str(),
        "Origin, Access-Control-Request-Method, Access-Control-Request-Headers".to_owned(),
    );
    insert(
        out,
        gts::HEADER_ERROR_SOURCE,
        gts::ERROR_SOURCE_GATEWAY.to_owned(),
    );
    response
}

/// Validates an actual cross-origin request against a CORS configuration.
///
/// Requests without an `Origin` are never CORS-checked.
///
/// # Errors
/// Returns [`DomainError::CorsOriginNotAllowed`] or
/// [`DomainError::CorsMethodNotAllowed`].
pub fn validate_request(
    cors: &CorsConfig,
    method: &Method,
    headers: &HeaderMap,
) -> Result<(), DomainError> {
    let Some(origin) = headers
        .get(http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
    else {
        return Ok(());
    };
    let origin = origin.trim();
    if origin.is_empty() {
        return Ok(());
    }
    if !cors.allowed_origins.iter().any(|o| o == "*" || o == origin) {
        return Err(DomainError::CorsOriginNotAllowed(origin.to_owned()));
    }
    if !cors
        .allowed_methods
        .iter()
        .any(|m| m.eq_ignore_ascii_case(method.as_str()))
    {
        return Err(DomainError::CorsMethodNotAllowed(
            method.as_str().to_owned(),
        ));
    }
    Ok(())
}

/// Adds the CORS response headers for an accepted cross-origin request.
///
/// `origin` is the `Origin` value already validated by [`validate_request`];
/// it is passed explicitly rather than round-tripped through a header.
pub fn apply_origin_headers(headers: &mut HeaderMap, cors: &CorsConfig, origin: &str) {
    if origin.is_empty() {
        return;
    }
    if let Ok(value) = http::HeaderValue::from_str(origin) {
        headers.insert(http::header::ACCESS_CONTROL_ALLOW_ORIGIN, value);
        headers.insert(http::header::VARY, http::HeaderValue::from_static("Origin"));
    }
    if cors.allow_credentials {
        headers.insert(
            http::header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
            http::HeaderValue::from_static("true"),
        );
    }
    if !cors.expose_headers.is_empty()
        && let Ok(value) = http::HeaderValue::from_str(&cors.expose_headers.join(", "))
    {
        headers.insert(http::header::ACCESS_CONTROL_EXPOSE_HEADERS, value);
    }
}
