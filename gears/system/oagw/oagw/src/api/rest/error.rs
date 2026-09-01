// Created: 2026-08-29 by Constructor Tech
//! Wire error mapping: RFC 9457 problem documents plus the
//! `X-OAGW-Error-Source` header (ADR-0007).

use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::domain::error::{OagwError, error_type};

/// Gateway-originated error, ready for the wire.
///
/// Wraps the domain error with the request-scoped problem members and the
/// routing context needed by the problem extensions. The members live behind a
/// single allocation so the error stays cheap to move through handler
/// `Result`s.
#[derive(Debug, Clone)]
pub struct ApiError {
    /// Domain error.
    pub kind: OagwError,
    /// Request-scoped problem members.
    pub context: Box<ProblemContext>,
    /// `X-OAGW-Error-Source` override for errors reported on a live stream.
    pub source: Option<String>,
}

/// Request-scoped members of a problem document.
#[derive(Debug, Clone, Default)]
pub struct ProblemContext {
    /// RFC 9457 `instance` (request path).
    pub instance: Option<String>,
    /// Distributed tracing correlation id.
    pub trace_id: Option<String>,
    /// Upstream id the request was routed to.
    pub upstream_id: Option<String>,
    /// Upstream host (alias) the request targeted.
    pub host: Option<String>,
}

impl ApiError {
    /// Wrap a domain error.
    #[must_use]
    pub fn new(kind: OagwError) -> Self {
        Self {
            kind,
            context: Box::default(),
            source: None,
        }
    }

    /// Set the RFC 9457 `instance`.
    #[must_use]
    pub fn with_instance(mut self, instance: impl Into<String>) -> Self {
        self.context.instance = Some(instance.into());
        self
    }

    /// Set the `trace_id`.
    #[must_use]
    pub fn with_trace_id(mut self, trace_id: impl Into<String>) -> Self {
        self.context.trace_id = Some(trace_id.into());
        self
    }

    /// Attach upstream routing context.
    #[must_use]
    pub fn with_upstream(
        mut self,
        upstream_id: impl Into<String>,
        host: impl Into<String>,
    ) -> Self {
        self.context.upstream_id = Some(upstream_id.into());
        self.context.host = Some(host.into());
        self
    }

    /// Set the error source header explicitly (used by the streaming paths).
    #[must_use]
    pub fn with_source_header(mut self, value: &str) -> Self {
        if axum::http::HeaderValue::from_str(value).is_ok() {
            self.source = Some(value.to_owned());
        }
        self
    }
}

impl From<OagwError> for ApiError {
    fn from(kind: OagwError) -> Self {
        Self::new(kind)
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.kind)
    }
}

impl std::error::Error for ApiError {}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let extras = self.kind.extras();
        let mut body = json!({
            "type": error_type(self.kind.type_suffix()),
            "title": self.kind.title(),
            "status": self.kind.status(),
            "detail": self.kind.detail(),
        });
        // DESIGN's error table publishes a Retriable column; surfacing it on
        // the document spares clients a status-code table of their own.
        body["retriable"] = json!(self.kind.retriable());
        if let Some(instance) = &self.context.instance {
            body["instance"] = json!(instance);
            body["path"] = json!(instance);
        }
        if let Some(trace_id) = &self.context.trace_id {
            body["trace_id"] = json!(trace_id);
        }
        if let Some(upstream_id) = self
            .context
            .upstream_id
            .as_ref()
            .or(extras.upstream_id.as_ref())
        {
            body["upstream_id"] = json!(upstream_id);
        }
        if let Some(host) = self.context.host.as_ref().or(extras.host.as_ref()) {
            body["host"] = json!(host);
        }
        if let Some(retry) = extras.retry_after_seconds {
            body["retry_after_seconds"] = json!(retry);
        }
        if let Some(valid_hosts) = extras.valid_hosts {
            body["valid_hosts"] = json!(valid_hosts);
        }
        if let Some(alias) = extras.alias {
            body["alias"] = json!(alias);
        }
        if let Some(invalid_value) = extras.invalid_value {
            body["invalid_value"] = json!(invalid_value);
        }
        if let Some(references) = extras.referenced_by {
            body["referenced_by"] = json!({
                "upstreams": references.upstreams,
                "routes": references.routes,
            });
        }

        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/problem+json"),
        );
        let source = self
            .source
            .and_then(|value| header::HeaderValue::from_str(&value).ok())
            .unwrap_or_else(|| header::HeaderValue::from_static("gateway"));
        headers.insert(
            header::HeaderName::from_static("x-oagw-error-source"),
            source,
        );
        if let Some(retry) = extras.retry_after_seconds
            && let Ok(value) = header::HeaderValue::from_str(&retry.to_string())
        {
            headers.insert(header::RETRY_AFTER, value);
        }

        (
            StatusCode::from_u16(self.kind.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            headers,
            body.to_string(),
        )
            .into_response()
    }
}

/// Extract a trace id from request headers, falling back to a generated UUID.
///
/// Security: reads only the two correlation headers; no request body, query
/// parameter or other header value is ever logged.
#[must_use]
pub fn trace_id_from_headers(headers: &HeaderMap) -> String {
    const CANDIDATES: [&str; 2] = ["x-request-id", "traceparent"];
    for candidate in CANDIDATES {
        if let Some(value) = headers.get(candidate)
            && let Ok(text) = value.to_str()
        {
            let trimmed = text.trim();
            let id = if candidate == "traceparent" {
                trimmed.split('-').next_back().unwrap_or(trimmed)
            } else {
                trimmed
            };
            if !id.is_empty() {
                return id.to_owned();
            }
        }
    }
    uuid::Uuid::new_v4().to_string()
}

/// `true` for errors reported through an already-started stream.
#[must_use]
pub fn is_streaming_error(error: &OagwError) -> bool {
    matches!(error, OagwError::StreamAborted(_))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limit_error_sets_retry_after() {
        let response = ApiError::new(OagwError::RateLimitExceeded(15)).into_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let headers = response.headers();
        assert_eq!(
            headers
                .get("x-oagw-error-source")
                .and_then(|v| v.to_str().ok()),
            Some("gateway")
        );
        assert_eq!(
            headers.get("retry-after").and_then(|v| v.to_str().ok()),
            Some("15")
        );
        assert_eq!(
            headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/problem+json")
        );
    }

    #[test]
    fn type_suffix_is_gts_shaped() {
        let error = OagwError::RouteNotFound("no route".to_owned());
        assert_eq!(
            error_type(error.type_suffix()),
            "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
        );
    }

    #[tokio::test]
    async fn problem_body_has_extension_members() {
        let response = ApiError::new(OagwError::MissingTargetHost {
            valid_hosts: vec!["a.example.com".to_owned()],
            alias: "vendor.com".to_owned(),
        })
        .with_instance("/oagw/v1/proxy/vendor.com")
        .into_response();
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body");
        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(value["status"], 400);
        assert_eq!(value["valid_hosts"], json!(["a.example.com"]));
        assert_eq!(value["alias"], "vendor.com");
        assert_eq!(value["instance"], "/oagw/v1/proxy/vendor.com");
        assert_eq!(value["path"], "/oagw/v1/proxy/vendor.com");
    }

    #[test]
    fn retriable_matches_the_design_error_table() {
        for (error, expected) in [
            (OagwError::RateLimitExceeded(1), true),
            (OagwError::LinkUnavailable("x".to_owned()), true),
            (OagwError::CircuitBreakerOpen, true),
            (OagwError::Validation("x".to_owned()), false),
            (OagwError::RouteNotFound("x".to_owned()), false),
            (OagwError::PayloadTooLarge, false),
        ] {
            let response = ApiError::new(error).into_response();
            let bytes = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("runtime")
                .block_on(axum::body::to_bytes(response.into_body(), 64 * 1024))
                .expect("body");
            let value: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
            assert_eq!(value["retriable"], expected, "{}", value["detail"]);
        }
    }
}
