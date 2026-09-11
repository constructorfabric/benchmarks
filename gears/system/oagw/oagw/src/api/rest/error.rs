//! Problem-document rendering for OAGW errors.
//!
//! Every gateway-generated error is RFC 9457 `application/problem+json` with
//! the GTS `type` identifier from `docs/DESIGN.md` and the
//! `X-OAGW-Error-Source: gateway` header (ADR 0007). Responses produced by an
//! upstream are never touched by this module: they are passed through with
//! `X-OAGW-Error-Source: upstream` instead.

use axum::http::HeaderValue;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::domain::error::DomainError;
use crate::domain::model::Upstream;

/// Media type of every gateway-generated error body.
pub const APPLICATION_PROBLEM_JSON: &str = "application/problem+json";

/// Header distinguishing a gateway error from a passed-through upstream
/// response.
pub const ERROR_SOURCE_HEADER: &str = "X-OAGW-Error-Source";

/// `X-OAGW-Error-Source` value for errors the gateway generated.
pub const ERROR_SOURCE_GATEWAY: &str = "gateway";

/// `X-OAGW-Error-Source` value for responses the upstream produced.
pub const ERROR_SOURCE_UPSTREAM: &str = "upstream";

/// A gateway error rendered as a problem document.
#[derive(Debug, Clone)]
pub struct GatewayProblem {
    error: DomainError,
    instance: String,
    upstream_id: Option<String>,
    host: Option<String>,
    path: Option<String>,
    trace_id: Option<String>,
    extra_headers: http::HeaderMap,
}

impl GatewayProblem {
    /// Wrap a domain error.
    #[must_use]
    pub fn new(error: DomainError) -> Self {
        Self {
            error,
            instance: String::new(),
            upstream_id: None,
            host: None,
            path: None,
            trace_id: None,
            extra_headers: http::HeaderMap::new(),
        }
    }

    /// Set the problem `instance` (the request URI).
    #[must_use]
    pub fn with_instance(mut self, instance: impl Into<String>) -> Self {
        self.instance = instance.into();
        self
    }

    /// Attach the upstream context the error occurred on.
    #[must_use]
    pub fn with_upstream(mut self, upstream: &Upstream) -> Self {
        self.upstream_id = Some(upstream.id.to_string());
        self.host = Some(upstream.alias.clone());
        self
    }

    /// Carry the headers the plugin chain asked for onto the response.
    ///
    /// A plugin's `transform_error` may attach a correlation identifier or a
    /// hint; the rendered problem document must carry them, so they travel with
    /// the failure until it is rendered here.
    #[must_use]
    pub fn with_headers(mut self, headers: http::HeaderMap) -> Self {
        self.extra_headers = headers;
        self
    }

    /// Attach the upstream identifier alone.
    #[must_use]
    pub fn with_upstream_id(mut self, upstream_id: impl Into<String>) -> Self {
        self.upstream_id = Some(upstream_id.into());
        self
    }

    /// Attach the upstream host.
    #[must_use]
    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.host = Some(host.into());
        self
    }

    /// Attach the forwarded path.
    #[must_use]
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// Attach the correlation identifier.
    #[must_use]
    pub fn with_trace_id(mut self, trace_id: impl Into<String>) -> Self {
        self.trace_id = Some(trace_id.into());
        self
    }

    /// The underlying domain error.
    #[must_use]
    pub const fn error(&self) -> &DomainError {
        &self.error
    }

    /// RFC 9457 document body.
    #[must_use]
    pub fn body(&self) -> serde_json::Value {
        let status = self.error.http_status().as_u16();
        // Assembled as a map so the document is never anything but an object.
        let mut obj = serde_json::Map::new();
        obj.insert("type".to_owned(), json!(self.error.gts_type()));
        obj.insert("title".to_owned(), json!(self.error.title()));
        obj.insert("status".to_owned(), json!(status));
        obj.insert("detail".to_owned(), json!(self.error.detail()));
        if !self.instance.is_empty() {
            obj.insert("instance".to_owned(), json!(self.instance));
        }
        if let Some(upstream_id) = &self.upstream_id {
            obj.insert("upstream_id".to_owned(), json!(upstream_id));
        }
        if let Some(host) = &self.host {
            obj.insert("host".to_owned(), json!(host));
        }
        if let Some(path) = &self.path {
            obj.insert("path".to_owned(), json!(path));
        }
        if let Some(retry_after) = self.error.retry_after() {
            obj.insert("retry_after_seconds".to_owned(), json!(retry_after));
        }
        if let Some(trace_id) = &self.trace_id {
            obj.insert("trace_id".to_owned(), json!(trace_id));
        }
        serde_json::Value::Object(obj)
    }

    /// Serialize the body.
    #[must_use]
    pub fn body_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(&self.body()).unwrap_or_default()
    }
}

impl From<DomainError> for GatewayProblem {
    fn from(error: DomainError) -> Self {
        Self::new(error)
    }
}

impl IntoResponse for GatewayProblem {
    fn into_response(self) -> Response {
        let status = self.error.http_status();
        let mut builder = Response::builder()
            .status(status)
            .header(http::header::CONTENT_TYPE, APPLICATION_PROBLEM_JSON)
            .header(ERROR_SOURCE_HEADER, ERROR_SOURCE_GATEWAY);
        for (name, value) in &self.extra_headers {
            if let (Ok(name), Ok(value)) = (
                http::HeaderName::from_bytes(name.as_str().as_bytes()),
                HeaderValue::from_bytes(value.as_bytes()),
            ) {
                builder = builder.header(name, value);
            }
        }
        let hint = self
            .error
            .retry_after()
            .and_then(|retry| HeaderValue::from_str(&retry.to_string()).ok());
        if let Some(value) = hint {
            builder = builder.header(http::header::RETRY_AFTER, value);
        }

        match builder.body(axum::body::Body::from(self.body_bytes())) {
            Ok(response) => response,
            // Body construction from owned bytes cannot fail; fall back to a
            // bare status rather than panicking in the error path.
            Err(_) => status.into_response(),
        }
    }
}

/// Result alias for handlers that render errors as problem documents.
pub type ApiResult<T> = Result<T, GatewayProblem>;

#[cfg(test)]
mod error_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::domain::error::ErrorKind;
    use http::header::CONTENT_TYPE;

    fn problem(kind: ErrorKind) -> GatewayProblem {
        GatewayProblem::from(DomainError::new(kind, "detail text".to_owned()))
            .with_instance("/oagw/v1/proxy/api.partner.com/v1/x")
            .with_host("api.partner.com")
            .with_path("/v1/x")
            .with_trace_id("trace-1")
    }

    #[test]
    fn body_carries_the_rfc9457_fields() {
        let p = problem(ErrorKind::RouteNotFound);
        let body = p.body();
        assert_eq!(body["type"], crate::gts_helpers::ERR_ROUTE_NOT_FOUND);
        assert_eq!(body["title"], "Route not found");
        assert_eq!(body["status"], 404);
        assert_eq!(body["detail"], "detail text");
        assert_eq!(body["instance"], "/oagw/v1/proxy/api.partner.com/v1/x");
        assert_eq!(body["host"], "api.partner.com");
        assert_eq!(body["path"], "/v1/x");
        assert_eq!(body["trace_id"], "trace-1");
    }

    #[test]
    fn rate_limit_problem_carries_retry_after() {
        let p = GatewayProblem::from(DomainError::rate_limit_exceeded("slow down", 12));
        assert_eq!(p.body()["retry_after_seconds"], 12);
        assert_eq!(p.body()["status"], 429);
    }

    #[test]
    fn every_gateway_error_sets_the_source_header() {
        for kind in [
            ErrorKind::Validation,
            ErrorKind::RouteNotFound,
            ErrorKind::MissingTargetHost,
            ErrorKind::RateLimitExceeded,
            ErrorKind::SecretNotFound,
            ErrorKind::AuthFailed,
        ] {
            let response = problem(kind).into_response();
            assert_eq!(
                response
                    .headers()
                    .get(ERROR_SOURCE_HEADER)
                    .and_then(|v| v.to_str().ok()),
                Some(ERROR_SOURCE_GATEWAY),
                "{kind:?} must be marked as a gateway error"
            );
            assert_eq!(
                response
                    .headers()
                    .get(CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok()),
                Some(APPLICATION_PROBLEM_JSON)
            );
        }
    }

    #[test]
    fn secret_not_found_is_an_internal_error() {
        let p = problem(ErrorKind::SecretNotFound);
        assert_eq!(
            p.error().http_status(),
            http::StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(p.body()["type"], crate::gts_helpers::ERR_SECRET_NOT_FOUND);
        assert!(!p.error().retriable());
    }
}
