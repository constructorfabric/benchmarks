//! RFC 9457 problem-details rendering for gateway errors (DESIGN §Error Response Format).
//!
//! OAGW renders its own bodies rather than going through the platform canonical-error catalogue:
//! the catalogue's GTS type ids are the platform's, not `cf.oagw.*`, and it cannot carry the OAGW
//! extension fields. Because the api-gateway's `canonical_error_middleware` re-serializes
//! `application/problem+json` bodies through the platform `Problem` struct — preserving `context`
//! but dropping unknown top-level keys — every extension field is emitted **both** at the top level
//! and mirrored inside `context`, so it stays reachable on the wire either way.

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};

use crate::domain::error::{DomainError, ProblemMeta};

/// Media type of every gateway error body.
pub const PROBLEM_MEDIA_TYPE: &str = "application/problem+json";

/// Header distinguishing gateway-generated from passthrough errors (ADR-0007).
pub const ERROR_SOURCE_HEADER: &str = "x-oagw-error-source";

/// Value stamped on gateway-generated errors.
pub const SOURCE_GATEWAY: &str = "gateway";
/// Value stamped on responses passed through from the upstream.
pub const SOURCE_UPSTREAM: &str = "upstream";

fn source_header_name() -> header::HeaderName {
    header::HeaderName::from_static(ERROR_SOURCE_HEADER)
}

/// Render `error` as an RFC 9457 problem response.
#[must_use]
pub fn problem_response(error: &DomainError, meta: &ProblemMeta, instance: &str) -> Response {
    let status = error.status();
    let body = problem_body(error, meta, instance, None);
    let mut response = (status, axum::Json(body)).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(PROBLEM_MEDIA_TYPE),
    );
    // Retriable errors also carry the header form of the retry guidance (PRD §Error Handling,
    // ADR-0007 Appendix A).
    if let Some(seconds) = error.meta_with(meta.clone()).retry_after_seconds
        && let Ok(value) = HeaderValue::from_str(&seconds.to_string())
    {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    stamp_gateway_source(&mut response);
    response
}

/// Render the problem body as JSON.
#[must_use]
pub fn problem_body(
    error: &DomainError,
    meta: &ProblemMeta,
    instance: &str,
    trace_id: Option<&str>,
) -> Value {
    let extensions = error.extensions_json(meta);
    let mut map = Map::new();
    map.insert("type".to_string(), json!(error.problem_type()));
    map.insert("title".to_string(), json!(error.title()));
    map.insert("status".to_string(), json!(error.status().as_u16()));
    map.insert("detail".to_string(), json!(error.detail()));
    map.insert("instance".to_string(), json!(instance));
    if let Some(trace) = trace_id {
        map.insert("trace_id".to_string(), json!(trace));
    }
    if let Value::Object(extra) = &extensions {
        for (k, v) in extra {
            map.insert(k.clone(), v.clone());
        }
    }
    map.insert("context".to_string(), extensions);
    Value::Object(map)
}

/// Stamp `X-OAGW-Error-Source: gateway` on a response that does not carry it yet.
pub fn stamp_gateway_source(response: &mut Response) {
    stamp_source(response, SOURCE_GATEWAY);
}

/// Stamp `X-OAGW-Error-Source: upstream` on a passthrough response.
pub fn stamp_upstream_source(response: &mut Response) {
    stamp_source(response, SOURCE_UPSTREAM);
}

fn stamp_source(response: &mut Response, source: &str) {
    if let Ok(value) = HeaderValue::from_str(source) {
        response.headers_mut().insert(source_header_name(), value);
    }
}

/// Read the error-source stamp off a response, if it carries one.
#[must_use]
pub fn source_of(response: &Response) -> Option<String> {
    response
        .headers()
        .get(source_header_name())
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// `X-OAGW-Error-Source` stamping layer: fills the header in on every response that lacks it.
///
/// Success responses and upstream passthroughs set it themselves; anything that reaches this layer
/// without it is a gateway-generated error.
pub fn error_source_layer(response: Response) -> Response {
    let (mut parts, body) = response.into_parts();
    if !parts.headers.contains_key(source_header_name())
        && let Ok(value) = HeaderValue::from_str(SOURCE_GATEWAY)
    {
        parts.headers.insert(source_header_name(), value);
    }
    Response::from_parts(parts, body)
}

/// Status-code helper for handlers that need the numeric value of an error.
#[must_use]
pub fn status_code(error: &DomainError) -> u16 {
    StatusCode::as_u16(&error.status())
}

#[cfg(test)]
#[path = "problem_tests.rs"]
mod tests;
