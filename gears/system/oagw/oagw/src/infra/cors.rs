//! CORS answering for the proxy.
//!
//! The gateway answers preflights itself and stamps the actual request's
//! response with the allow-origin header. When no CORS configuration applies,
//! the answer is permissive: the gateway is a passthrough and a browser that
//! sent a preflight expects a decision, not a refusal the upstream never asked
//! for.

use crate::domain::merge::EffectiveConfig;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

/// `Access-Control-Max-Age` applied to preflight answers, in seconds.
pub const PREFLIGHT_MAX_AGE: &str = "600";

/// Answers a CORS preflight from an effective configuration, if any.
#[must_use]
pub fn preflight(config: Option<&EffectiveConfig>, request_headers: &HeaderMap) -> Response {
    let mut builder = Response::builder().status(StatusCode::NO_CONTENT);
    let origins = config.map_or_else(Vec::new, |effective| effective.cors_origins.clone());
    let methods = config.map_or_else(Vec::new, |effective| effective.cors_methods.clone());
    let requested = request_headers
        .get(header::ACCESS_CONTROL_REQUEST_METHOD)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let requested_headers = request_headers
        .get(header::ACCESS_CONTROL_REQUEST_HEADERS)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();

    builder = allow_origin(builder, &origins, request_headers);
    let allow_methods = if methods.is_empty() {
        requested
    } else {
        methods.join(", ")
    };
    if !allow_methods.is_empty() {
        builder = builder.header(header::ACCESS_CONTROL_ALLOW_METHODS, allow_methods);
    }
    if !requested_headers.is_empty() {
        builder = builder.header(header::ACCESS_CONTROL_ALLOW_HEADERS, requested_headers);
    }
    builder = builder.header(header::ACCESS_CONTROL_MAX_AGE, PREFLIGHT_MAX_AGE);
    builder
        .body(axum::body::Body::empty())
        .unwrap_or_else(|_| StatusCode::NO_CONTENT.into_response())
}

/// Whether an actual request may proceed, and the headers to stamp on it.
///
/// Returns the `Access-Control-Allow-Origin` value to send, or `None` when the
/// origin is not allowed. No configuration at all is permissive.
#[must_use]
pub fn actual_request(
    config: Option<&EffectiveConfig>,
    request_headers: &HeaderMap,
) -> Option<(String, Vec<String>)> {
    let origin = request_headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)?;
    let Some(config) = config else {
        return Some((origin, Vec::new()));
    };
    if !config.cors_enabled {
        return Some((origin, config.cors_expose_headers.clone()));
    }
    let allowed = config
        .cors_origins
        .iter()
        .any(|candidate| candidate == "*" || candidate == &origin);
    if !allowed {
        return None;
    }
    // The gateway polices the origin only; the method list is advisory.
    Some((origin, config.cors_expose_headers.clone()))
}

/// The value to expose for `Access-Control-Allow-Origin` on a response.
fn allow_origin(
    builder: axum::http::response::Builder,
    origins: &[String],
    request_headers: &HeaderMap,
) -> axum::http::response::Builder {
    let echoed = request_headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    match (origins.is_empty(), echoed) {
        (true, Some(origin)) => builder.header(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin),
        (false, Some(_origin)) if origins.iter().any(|candidate| candidate == "*") => {
            builder.header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        }
        (false, Some(origin)) if origins.iter().any(|candidate| candidate == &origin) => {
            builder.header(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin)
        }
        // No origin, no configuration: the answer is permissive and the header
        // stays off, which is what a non-browser caller expects.
        _ => builder,
    }
}

/// Stamps the expose-headers header onto a response when the list is not empty.
pub fn stamp_expose_headers(
    builder: axum::http::response::Builder,
    expose: &[String],
) -> axum::http::response::Builder {
    if expose.is_empty() {
        return builder;
    }
    builder.header(header::ACCESS_CONTROL_EXPOSE_HEADERS, expose.join(", "))
}

/// Whether a header value is a legal single-line header value.
#[must_use]
pub fn is_valid_value(value: &str) -> bool {
    HeaderValue::from_str(value).is_ok()
}
