//! REST error rendering for the `oagw` gear.
//!
//! Gateway errors are rendered as RFC 9457 problem bodies carrying the gear's
//! own GTS `type` identifiers (`gts.cf.core.errors.err.v1~cf.oagw.<error>.v1`)
//! and the `X-OAGW-Error-Source: gateway` header, per ADR-0007. The bodies are
//! self-contained: `instance` and `trace_id` are filled here, so the platform
//! canonical-error middleware (which only rewrites bodies it can deserialize
//! into its own `Problem` shape) passes them through unchanged.

use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};

use crate::domain::error::DomainError;

/// Response header distinguishing gateway from upstream errors (ADR-0007).
pub const ERROR_SOURCE_HEADER: &str = "X-OAGW-Error-Source";

/// Value of [`ERROR_SOURCE_HEADER`] for errors produced by the gateway.
pub const ERROR_SOURCE_GATEWAY: &str = "gateway";

/// Value of [`ERROR_SOURCE_HEADER`] for errors passed through from upstream.
pub const ERROR_SOURCE_UPSTREAM: &str = "upstream";

/// Content type of a problem body.
pub const PROBLEM_JSON: &str = "application/problem+json";

/// Builds the RFC 9457 problem body for a gateway error.
///
/// Extension fields are flattened at the top level next to the standard
/// fields, exactly as the examples in ADR-0007 and ADR-0001 render them.
#[must_use]
pub fn problem_body(err: &DomainError) -> Value {
    let mut body = Map::new();
    body.insert("type".to_owned(), json!(err.kind.gts_type()));
    body.insert("title".to_owned(), json!(err.kind.title()));
    body.insert("status".to_owned(), json!(err.kind.status()));
    body.insert("detail".to_owned(), json!(err.detail));
    body.insert("retriable".to_owned(), json!(err.kind.retriable()));
    body.insert("error_source".to_owned(), json!(ERROR_SOURCE_GATEWAY));
    for (name, value) in &err.fields {
        body.insert((*name).to_owned(), value.clone());
    }
    Value::Object(body)
}

/// Extracts the trace id from request headers with the same precedence the
/// platform canonical-error middleware uses (`traceparent` → `x-trace-id` →
/// `x-request-id`).
#[must_use]
pub fn request_trace_id(headers: &HeaderMap) -> Option<String> {
    if let Some(tp) = headers
        .get("traceparent")
        .and_then(|v| v.to_str().ok())
        .and_then(parse_w3c_trace_id)
    {
        return Some(tp);
    }
    for name in ["x-trace-id", "x-request-id"] {
        if let Some(v) = headers.get(name).and_then(|v| v.to_str().ok()) {
            return Some(v.to_owned());
        }
    }
    None
}

/// The request path, used as the problem `instance`.
#[must_use]
pub fn request_instance(uri: &Uri) -> String {
    uri.path().to_owned()
}

/// Fills `instance` and `trace_id` into a problem body in place.
pub fn add_request_context(body: &mut Map<String, Value>, uri: &Uri, headers: &HeaderMap) {
    body.entry("instance".to_owned())
        .or_insert_with(|| json!(request_instance(uri)));
    if let Some(trace_id) = request_trace_id(headers) {
        body.entry("trace_id".to_owned())
            .or_insert_with(|| json!(trace_id));
    }
}

/// Renders a gateway error as the full error response.
#[must_use]
pub fn error_response(err: &DomainError) -> Response {
    error_response_with_context(err, &Uri::default(), &HeaderMap::new())
}

/// Renders an error response with request context attached.
#[must_use]
pub fn error_response_with_context(err: &DomainError, uri: &Uri, headers: &HeaderMap) -> Response {
    let mut body = problem_body(err);
    if let Some(obj) = body.as_object_mut() {
        add_request_context(obj, uri, headers);
    }
    render(err.status(), err.headers.clone(), body)
}

fn render(status: u16, extra_headers: Vec<(String, String)>, body: Value) -> Response {
    let payload = serde_json::to_vec(&body).unwrap_or_default();
    let mut response = Response::new(Body::from(payload));
    *response.status_mut() =
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    if let Ok(value) = HeaderValue::from_str(PROBLEM_JSON) {
        response.headers_mut().insert(header::CONTENT_TYPE, value);
    }
    let gateway = HeaderValue::from_static(ERROR_SOURCE_GATEWAY);
    response
        .headers_mut()
        .insert(HeaderName::from_static("x-oagw-error-source"), gateway);
    for (name, value) in extra_headers {
        if let (Ok(name), Ok(value)) = (HeaderName::try_from(name), HeaderValue::from_str(&value)) {
            response.headers_mut().insert(name, value);
        }
    }
    response
}

/// A gateway error rendered as the gear's own problem+json response.
#[derive(Debug, Clone)]
pub struct OagwError(pub DomainError);

impl OagwError {
    /// Wraps a domain error.
    #[must_use]
    pub const fn new(err: DomainError) -> Self {
        Self(err)
    }
}

impl From<DomainError> for OagwError {
    fn from(err: DomainError) -> Self {
        Self(err)
    }
}

impl IntoResponse for OagwError {
    fn into_response(self) -> Response {
        error_response(&self.0)
    }
}

/// Renders an upstream passthrough response (ADR-0007) with the
/// `X-OAGW-Error-Source: upstream` header.
#[must_use]
pub fn upstream_response(
    status: u16,
    headers: Vec<(String, String)>,
    body: axum::body::Body,
) -> Response {
    render_passthrough(status, headers, body, ERROR_SOURCE_UPSTREAM)
}

/// Renders an answer the gateway produced itself (the CORS preflight) with
/// the `X-OAGW-Error-Source: gateway` header (ADR-0007).
#[must_use]
pub fn local_response(
    status: u16,
    headers: Vec<(String, String)>,
    body: axum::body::Body,
) -> Response {
    render_passthrough(status, headers, body, ERROR_SOURCE_GATEWAY)
}

fn render_passthrough(
    status: u16,
    headers: Vec<(String, String)>,
    body: axum::body::Body,
    source: &str,
) -> Response {
    let mut response = Response::new(body);
    if let Ok(code) = StatusCode::from_u16(status) {
        *response.status_mut() = code;
    }
    for (name, value) in headers {
        if let (Ok(name), Ok(value)) = (HeaderName::try_from(name), HeaderValue::from_str(&value)) {
            response.headers_mut().insert(name, value);
        }
    }
    let source = HeaderValue::from_str(source)
        .unwrap_or_else(|_| HeaderValue::from_static(ERROR_SOURCE_UPSTREAM));
    response
        .headers_mut()
        .insert(HeaderName::from_static("x-oagw-error-source"), source);
    response
}

fn parse_w3c_trace_id(traceparent: &str) -> Option<String> {
    let parts: Vec<&str> = traceparent.split('-').collect();
    if parts.len() >= 4 && parts[0] == "00" {
        return Some(parts[1].to_owned());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::error::ErrorKind;

    fn headers_of(response: &Response) -> HeaderMap {
        response.headers().clone()
    }

    #[test]
    fn problem_body_carries_gts_type_and_extensions() {
        let err = DomainError::new(ErrorKind::UnknownTargetHost, "no such host")
            .with_field("valid_hosts", json!(["us.example.com"]))
            .with_field("invalid_value", json!("apac.example.com"));
        let body = problem_body(&err);
        assert_eq!(
            body["type"],
            "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1"
        );
        assert_eq!(body["status"], 400);
        assert_eq!(body["detail"], "no such host");
        assert_eq!(body["valid_hosts"], json!(["us.example.com"]));
        assert_eq!(body["error_source"], "gateway");
        assert_eq!(body["retriable"], false);
    }

    #[test]
    fn error_response_sets_content_type_and_source_header() {
        let err = DomainError::new(ErrorKind::RateLimitExceeded, "slow down")
            .with_header("Retry-After".to_owned(), "3".to_owned());
        let response = error_response(&err);
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let headers = headers_of(&response);
        assert_eq!(
            headers.get("content-type").and_then(|v| v.to_str().ok()),
            Some(PROBLEM_JSON)
        );
        assert_eq!(
            headers
                .get("x-oagw-error-source")
                .and_then(|v| v.to_str().ok()),
            Some("gateway")
        );
        assert_eq!(
            headers.get("retry-after").and_then(|v| v.to_str().ok()),
            Some("3")
        );
    }

    #[test]
    fn request_context_is_attached() {
        let mut map = HeaderMap::new();
        map.insert("x-request-id", HeaderValue::from_static("req-42"));
        let mut body = problem_body(&DomainError::new(ErrorKind::Internal, "boom"));
        if let Some(obj) = body.as_object_mut() {
            add_request_context(obj, &Uri::from_static("/oagw/v1/proxy/a/b"), &map);
        }
        assert_eq!(body["instance"], "/oagw/v1/proxy/a/b");
        assert_eq!(body["trace_id"], "req-42");
    }

    #[test]
    fn traceparent_wins_over_request_id() {
        let mut map = HeaderMap::new();
        map.insert(
            "traceparent",
            HeaderValue::from_static("00-0af7651916cd43dd8448eb211c80319c-00f067aa0ba902b7-01"),
        );
        map.insert("x-request-id", HeaderValue::from_static("req-42"));
        assert_eq!(
            request_trace_id(&map).as_deref(),
            Some("0af7651916cd43dd8448eb211c80319c")
        );
    }

    #[test]
    fn upstream_responses_carry_the_upstream_source_header() {
        let response = upstream_response(500, Vec::new(), Body::empty());
        assert_eq!(
            headers_of(&response)
                .get("x-oagw-error-source")
                .and_then(|v| v.to_str().ok()),
            Some("upstream")
        );
        let ok = upstream_response(200, Vec::new(), Body::empty());
        assert_eq!(
            headers_of(&ok)
                .get("x-oagw-error-source")
                .and_then(|v| v.to_str().ok()),
            Some("upstream")
        );
    }
}
