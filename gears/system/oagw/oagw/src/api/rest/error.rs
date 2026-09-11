//! Problem+json mapping and the error-source header.
//!
//! Every response the gear produces — success or failure — carries
//! `X-OAGW-Error-Source`. Gateway-produced failures are RFC 9457 problem
//! documents with GTS `type` identifiers.

use crate::domain::error::OagwError;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

/// Header distinguishing gateway-originated from upstream-passthrough responses.
pub const ERROR_SOURCE_HEADER: &str = "X-OAGW-Error-Source";

/// Value carried when the gateway produced the response.
pub const ERROR_SOURCE_GATEWAY: &str = "gateway";

/// Value carried when the response is a passthrough from the upstream.
pub const ERROR_SOURCE_UPSTREAM: &str = "upstream";

/// RFC 9457 problem document with the OAGW extension members.
#[derive(Debug, Clone, Serialize)]
pub struct ProblemDocument {
    /// GTS type identifier for the error.
    #[serde(rename = "type")]
    pub type_id: String,
    /// Human-readable title.
    pub title: String,
    /// HTTP status.
    pub status: u16,
    /// What failed.
    pub detail: String,
    /// Request path the failure occurred on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    /// Correlation identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// Retry guidance, in seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u64>,
    /// Target upstream identifier, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_id: Option<String>,
    /// Target host, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Target path, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl ProblemDocument {
    /// Builds a document from an error and the request path it occurred on.
    #[must_use]
    pub fn from_error(err: &OagwError, instance: Option<&str>) -> Self {
        Self {
            type_id: err.type_id(),
            title: err.title().to_owned(),
            status: err.status(),
            detail: scrub(&err.detail()),
            instance: instance.map(std::borrow::ToOwned::to_owned),
            trace_id: None,
            retry_after_seconds: err.retry_after().map(|value| value.as_secs()),
            upstream_id: None,
            host: None,
            path: None,
        }
    }

    /// Sets the correlation identifier.
    #[must_use]
    pub fn with_trace_id(mut self, trace_id: Option<String>) -> Self {
        self.trace_id = trace_id;
        self
    }

    /// Sets the target upstream context.
    #[must_use]
    pub fn with_upstream(mut self, upstream_id: Option<String>, host: Option<String>) -> Self {
        self.upstream_id = upstream_id;
        self.host = host;
        self
    }

    /// Sets the target path context.
    #[must_use]
    pub fn with_path(mut self, path: Option<String>) -> Self {
        self.path = path;
        self
    }

    /// Serialises the document to JSON.
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| {
            format!(
                "{{\"type\":\"{}\",\"title\":\"{}\",\"status\":{}}}",
                self.type_id, self.title, self.status
            )
        })
    }
}

/// Marker substituted for credential material found in a detail message.
pub const REDACTED: &str = "[redacted]";

/// Scrubs credential-shaped material out of a detail message.
///
/// Details are built from `cred://` references only, so a value that reaches a
/// document means a producer slipped one in; the scrubber removes the shapes a
/// credential takes — a reference, a bearer token, a `sk-`-prefixed key —
/// instead of trusting every producer to have kept it out.
#[must_use]
pub fn scrub(detail: &str) -> String {
    let mut out = String::with_capacity(detail.len());
    let mut rest = detail;
    while let Some((start, length)) = next_marker(rest) {
        out.push_str(&rest[..start]);
        out.push_str(REDACTED);
        rest = &rest[start + length..];
    }
    out.push_str(rest);
    out
}

/// The next credential-shaped marker: its offset and the length to replace.
///
/// Resuming from the start of the remainder after each replacement is what
/// lets a marker nested inside a longer one (`cred://tenant/sk-key`) also go.
fn next_marker(rest: &str) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize)> = None;
    let mut consider = |candidate: Option<(usize, usize)>| {
        if candidate.is_some_and(|(start, _)| best.is_none_or(|(found, _)| start < found)) {
            best = candidate;
        }
    };
    for prefix in ["cred://", "Bearer "] {
        consider(rest.find(prefix).and_then(|start| {
            let value = &rest[start + prefix.len()..];
            let run = token_run(value);
            (run > 0).then_some((start, prefix.len() + run))
        }));
    }
    consider(rest.find("sk-").and_then(|start| {
        let run = token_run(&rest[start + 3..]);
        // A short run is more likely an ordinary word than a key.
        (run >= 6).then_some((start, 3 + run))
    }));
    best
}

/// The length of the token characters at the start of `value`.
fn token_run(value: &str) -> usize {
    value
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~'))
        .map(char::len_utf8)
        .sum()
}

/// Sets the error-source header on a response.
pub fn with_error_source(response: &mut Response, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        response.headers_mut().insert(ERROR_SOURCE_HEADER, value);
    }
}

/// Builds a gateway failure response from an error.
#[must_use]
pub fn gateway_error_response(err: &OagwError, instance: Option<&str>) -> Response {
    let status = StatusCode::from_u16(err.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let document = ProblemDocument::from_error(err, instance);
    let mut response = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/problem+json")
        .body(axum::body::Body::from(document.to_json()))
        .unwrap_or_default();
    with_error_source(&mut response, ERROR_SOURCE_GATEWAY);
    let retry_after = err
        .retry_after()
        .and_then(|value| HeaderValue::from_str(&value.as_secs().to_string()).ok());
    if let Some(value) = retry_after {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

/// Marks a response as gateway-produced.
#[must_use]
pub fn mark_gateway(mut response: Response) -> Response {
    with_error_source(&mut response, ERROR_SOURCE_GATEWAY);
    response
}

/// Marks a response as an upstream passthrough.
#[must_use]
pub fn mark_upstream(mut response: Response) -> Response {
    with_error_source(&mut response, ERROR_SOURCE_UPSTREAM);
    response
}

impl IntoResponse for OagwError {
    fn into_response(self) -> Response {
        gateway_error_response(&self, None)
    }
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;
