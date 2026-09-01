// Created: 2026-08-31 by Constructor Tech
//! Problem-response post-processing.
//!
//! OAGW owns its problem+json shape (no `context` member), so the toolkit's
//! `canonical_error_middleware` cannot fill `instance` / `trace_id` for it.
//! This middleware mirrors that behaviour for OAGW bodies only, and is applied
//! to the OAGW sub-router so no other gear's routes are affected.

use axum::body::{Body, to_bytes};
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::api::extract::BodyLimit;
use crate::error::OagwError;

const PROBLEM_JSON: &str = "application/problem+json";

/// Reject requests whose declared body exceeds the configured limit (413).
///
/// The toolkit `OoP` serve path installs no body-limit layer (unlike the API
/// gateway), so the gear enforces `oagw.config.max_body_bytes` itself, on the
/// management routes only: the proxy route of slice 2 must stay free to stream
/// large request bodies.
///
/// Declared lengths are rejected here before any buffering; a body that never
/// declares its length is cut off by [`crate::api::extract::JsonBody`].
pub async fn enforce_body_limit(request: Request, next: Next) -> Response {
    let Some(limit) = request.extensions().get::<BodyLimit>().map(|limit| limit.0) else {
        return next.run(request).await;
    };
    let Some(declared) = declared_length(&request) else {
        return next.run(request).await;
    };
    if declared > limit {
        return OagwError::payload_too_large(limit, declared).into_response();
    }
    next.run(request).await
}

/// Declared `Content-Length` of the request, if any.
fn declared_length(request: &Request) -> Option<u64> {
    request
        .headers()
        .get(header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
}

/// Fill `instance` (request path) and `trace_id` on OAGW problem responses.
///
/// Bodies that already carry the member are left untouched, so handlers stay
/// in control of the wire shape.
pub async fn enrich_problem_response(request: Request, next: Next) -> Response {
    let request_path = request.uri().path().to_owned();
    let request_headers = request.headers().clone();
    let response = next.run(request).await;

    if !is_oagw_problem(&response) {
        return response;
    }
    decorate_problem(response, &request_path, &request_headers).await
}

/// Post-process a single problem body.
async fn decorate_problem(response: Response, path: &str, request_headers: &HeaderMap) -> Response {
    let (parts, body) = response.into_parts();
    let Some(bytes) = read_problem_bytes(body).await else {
        return Response::from_parts(parts, Body::empty());
    };
    let Some(updated) = decorate_problem_json(&bytes, path, request_headers) else {
        return Response::from_parts(parts, Body::from(bytes));
    };
    let length = updated.len();
    let mut response = Response::from_parts(parts, Body::from(updated));
    response
        .headers_mut()
        .insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    response
}

/// Buffered body of a problem response.
async fn read_problem_bytes(body: Body) -> Option<Vec<u8>> {
    match to_bytes(body, usize::MAX).await {
        Ok(bytes) => Some(bytes.to_vec()),
        Err(error) => {
            tracing::error!(error = %error, "oagw problem middleware: body read failed");
            None
        }
    }
}

/// Add `instance` / `trace_id`, then re-encode.
///
/// `None` when the body is not a JSON object or cannot be re-encoded; the
/// original bytes are served unchanged in that case.
fn decorate_problem_json(bytes: &[u8], path: &str, request_headers: &HeaderMap) -> Option<Vec<u8>> {
    let mut problem: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let object = problem.as_object_mut()?;
    object
        .entry("instance")
        .or_insert_with(|| serde_json::Value::String(path.to_owned()));
    if !object.contains_key("trace_id")
        && let Some(trace_id) = extract_trace_id(request_headers)
    {
        object.insert("trace_id".to_owned(), serde_json::Value::String(trace_id));
    }
    warn_on_client_error(&problem);
    serde_json::to_vec(&problem).ok()
}

/// OAGW-owned 4xx observability.
///
/// The outer canonical-error middleware cannot parse OAGW bodies (they carry
/// no `context` member), so client errors are logged here instead of being
/// silently dropped.
fn warn_on_client_error(problem: &serde_json::Value) {
    let Some(status) = problem.get("status").and_then(serde_json::Value::as_u64) else {
        return;
    };
    if !(400..500).contains(&status) {
        return;
    }
    tracing::warn!(
        status = status,
        error_type = problem
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown"),
        "oagw rejected a client request"
    );
}

fn is_oagw_problem(response: &Response) -> bool {
    response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|content_type| content_type.starts_with(PROBLEM_JSON))
}

/// W3C `traceparent` → `x-trace-id` → `x-request-id` → span id fallback,
/// matching `toolkit::api::canonical_error_middleware`.
pub(crate) fn extract_trace_id(headers: &axum::http::HeaderMap) -> Option<String> {
    correlation_id(headers).or_else(|| {
        tracing::Span::current()
            .id()
            .map(|id| id.into_u64().to_string())
    })
}

/// Correlation id the request's own trace headers name, if any.
///
/// The span-id fallback is deliberately **not** here: a span id is only
/// meaningful to the subscriber that minted it, and the caller that holds the
/// span can read it off the handle, where it is cheaper and always live.
pub(crate) fn correlation_id(headers: &axum::http::HeaderMap) -> Option<String> {
    if let Some(traceparent) = headers
        .get("traceparent")
        .and_then(|value| value.to_str().ok())
        && let Some(trace_id) = parse_w3c_trace_id(traceparent)
    {
        return Some(trace_id);
    }
    for name in ["x-trace-id", "x-request-id"] {
        if let Some(value) = headers.get(name).and_then(|value| value.to_str().ok())
            && let Some(correlation) = bound_correlation(value)
        {
            return Some(correlation);
        }
    }
    None
}

/// The 32-hex trace-id segment of a W3C `traceparent` header.
fn parse_w3c_trace_id(traceparent: &str) -> Option<String> {
    let parts: Vec<&str> = traceparent.split('-').collect();
    let candidate = parts.get(1)?;
    (candidate.len() == 32 && candidate.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| (*candidate).to_owned())
}

/// Whether a correlation id taken from a request header may be recorded.
///
/// The two fallback headers are client input, and the value goes into a
/// structured field of a log record, so it is bounded the way
/// [`parse_w3c_trace_id`] bounds its segment: a length cap and printable ASCII
/// only. A value outside either bound names nothing the trail can correlate and
/// is dropped rather than copied.
fn bound_correlation(value: &str) -> Option<String> {
    let bounded = value.len() <= CORRELATION_CAP
        && !value.is_empty()
        && value
            .chars()
            .all(|character| character.is_ascii_graphic() || character == ' ');
    bounded.then(|| value.to_owned())
}

/// Longest correlation id a request header may contribute (a W3C trace id is
/// 32 characters; 128 leaves room for an id the platform minted itself).
const CORRELATION_CAP: usize = 128;

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::extract::Request;
    use axum::http::header;
    use axum::middleware::from_fn;
    use axum::routing::get;
    use tower::ServiceExt;

    use super::{enrich_problem_response, extract_trace_id, parse_w3c_trace_id};
    use crate::error::{ERROR_SOURCE_GATEWAY, ERROR_SOURCE_HEADER, OagwError};

    #[test]
    fn parses_a_w3c_traceparent() {
        let trace_id = "0af7651916cd43dd8448eb211c80319c";
        let header = format!("00-{trace_id}-b7ad6b7169203331-01");
        assert_eq!(parse_w3c_trace_id(&header).as_deref(), Some(trace_id));
        assert_eq!(parse_w3c_trace_id("00-short-b7ad6b7169203331-01"), None);
    }

    #[test]
    fn falls_back_to_request_headers() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            "x-request-id",
            axum::http::HeaderValue::from_static("req-42"),
        );
        assert_eq!(extract_trace_id(&headers).as_deref(), Some("req-42"));
    }

    /// The `OoP` serve path wraps every gear in
    /// `toolkit::api::canonical_error_middleware`; the OAGW problem contract
    /// (type/status/detail, extension members, `X-OAGW-Error-Source`) must come
    /// out of it unchanged rather than reshaped into a toolkit `Problem`.
    #[tokio::test]
    async fn the_oagw_problem_contract_survives_the_toolkit_middleware()
    -> Result<(), Box<dyn std::error::Error>> {
        async fn rejected() -> OagwError {
            OagwError::validation("tags must be lowercase '[a-z0-9_-]+' labels")
                .with_extension(|ext| ext.invalid_value = Some("Eu West".to_owned()))
        }

        let app = axum::Router::new()
            .route("/oagw/v1/plugins", get(rejected))
            .layer(from_fn(enrich_problem_response))
            .layer(from_fn(toolkit::api::canonical_error_middleware));
        let request = Request::builder()
            .method("GET")
            .uri("/oagw/v1/plugins")
            .body(Body::empty())?;
        let response = app.oneshot(request).await?;

        assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        assert!(content_type.starts_with("application/problem+json"));
        assert_eq!(
            response
                .headers()
                .get(ERROR_SOURCE_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some(ERROR_SOURCE_GATEWAY)
        );

        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
        let problem: serde_json::Value = serde_json::from_slice(&bytes)?;
        let error_type = problem
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        assert!(error_type.ends_with("validation.error.v1"), "{error_type}");
        assert_eq!(
            problem.get("status").and_then(serde_json::Value::as_u64),
            Some(400)
        );
        assert_eq!(
            problem
                .get("invalid_value")
                .and_then(serde_json::Value::as_str),
            Some("Eu West")
        );
        assert_eq!(
            problem.get("instance").and_then(serde_json::Value::as_str),
            Some("/oagw/v1/plugins")
        );
        // The canonical middleware bailed out instead of reshaping the body.
        assert_eq!(problem.get("context"), None);
        Ok(())
    }
}
