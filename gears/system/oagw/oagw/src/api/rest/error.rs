//! Error responses.
//!
//! Every gateway-generated response carries `application/problem+json` with
//! the oagw GTS instance identifier in `type` (DESIGN.md §3.3) and the
//! `X-OAGW-Error-Source: gateway` header (ADR-0007).

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use uuid::Uuid;

use crate::domain::error::DomainError;

/// `X-OAGW-Error-Source` value for a response the gateway produced.
pub const SOURCE_GATEWAY: &str = "gateway";

/// `X-OAGW-Error-Source` value for a response that came from the upstream.
pub const SOURCE_UPSTREAM: &str = "upstream";

/// The header that separates gateway errors from forwarded upstream errors.
pub const ERROR_SOURCE_HEADER: &str = "x-oagw-error-source";

/// Typed header name for [`ERROR_SOURCE_HEADER`].
pub const ERROR_SOURCE_HEADER_NAME: http::HeaderName =
    http::HeaderName::from_static("x-oagw-error-source");

/// A problem+json body.
#[derive(Debug, Serialize)]
pub struct ProblemBody {
    /// The GTS instance identifier.
    #[serde(rename = "type")]
    pub problem_type: String,
    /// Short human-readable summary.
    pub title: String,
    /// HTTP status.
    pub status: u16,
    /// Human-readable explanation.
    pub detail: String,
    /// The request path that produced the error.
    pub instance: String,
    /// Seconds after which a retry may succeed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u64>,
    /// The upstream the request resolved to, when one had.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_id: Option<Uuid>,
    /// The `Host` the caller addressed, when it sent one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// The proxied path, when the request was a proxy one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The correlation id the request settled on, when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

/// A [`DomainError`] ready to be written to the wire.
#[derive(Debug)]
pub struct OagwError {
    /// The error.
    pub error: DomainError,
    /// The request path, used as the problem `instance`.
    pub instance: String,
    /// Extra headers, such as `Retry-After`.
    pub extra_headers: Vec<(String, String)>,
    /// The upstream the request had resolved to.
    pub upstream_id: Option<Uuid>,
    /// The `Host` the caller sent.
    pub host: Option<String>,
    /// The proxied path.
    pub path: Option<String>,
    /// The correlation id the request settled on.
    pub trace_id: Option<String>,
}

impl OagwError {
    /// Wraps a domain error with an empty instance.
    #[must_use]
    pub fn new(error: DomainError) -> Self {
        Self {
            error,
            instance: String::new(),
            extra_headers: Vec::new(),
            upstream_id: None,
            host: None,
            path: None,
            trace_id: None,
        }
    }

    /// Sets the problem `instance`.
    #[must_use]
    pub fn with_instance(mut self, instance: impl Into<String>) -> Self {
        self.instance = instance.into();
        self
    }

    /// Names the upstream the request had resolved to.
    #[must_use]
    pub fn with_upstream_id(mut self, upstream_id: Uuid) -> Self {
        self.upstream_id = Some(upstream_id);
        self
    }

    /// Records the `Host` the caller sent.
    #[must_use]
    pub fn with_host(mut self, host: Option<String>) -> Self {
        self.host = host;
        self
    }

    /// Records the proxied path.
    #[must_use]
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// Records the correlation id the request settled on.
    #[must_use]
    pub fn with_trace_id(mut self, trace_id: Option<String>) -> Self {
        self.trace_id = trace_id;
        self
    }

    /// Adds a header to the response.
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_headers.push((name.into(), value.into()));
        self
    }
}

impl From<DomainError> for OagwError {
    fn from(error: DomainError) -> Self {
        Self::new(error)
    }
}

impl From<crate::infra::proxy::service::ProxyFailure> for OagwError {
    fn from(failure: crate::infra::proxy::service::ProxyFailure) -> Self {
        Self {
            error: failure.error,
            instance: String::new(),
            extra_headers: failure.extra_headers,
            upstream_id: failure.upstream_id,
            host: None,
            path: None,
            trace_id: None,
        }
    }
}

impl IntoResponse for OagwError {
    fn into_response(self) -> Response {
        let descriptor = self.error.descriptor();
        let status =
            StatusCode::from_u16(descriptor.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = ProblemBody {
            problem_type: descriptor.gts_type,
            title: descriptor.title,
            status: descriptor.status,
            detail: self.error.to_string(),
            instance: self.instance.clone(),
            retry_after_seconds: retry_after(&self.extra_headers),
            upstream_id: self.upstream_id,
            host: self.host,
            path: self.path,
            trace_id: self.trace_id,
        };
        let mut response = (status, Json(body)).into_response();
        response.headers_mut().insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/problem+json"),
        );
        response
            .headers_mut()
            .insert(ERROR_SOURCE_HEADER_NAME, http::HeaderValue::from_static(SOURCE_GATEWAY));
        for (name, value) in &self.extra_headers {
            if let (Ok(name), Ok(value)) = (
                http::HeaderName::from_bytes(name.as_bytes()),
                http::HeaderValue::from_str(value),
            ) {
                response.headers_mut().insert(name, value);
            }
        }
        response
    }
}

fn retry_after(extra: &[(String, String)]) -> Option<u64> {
    extra
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))
        .and_then(|(_, value)| value.parse().ok())
}

/// Adds `X-OAGW-Error-Source: upstream` to a forwarded response's headers.
pub fn mark_upstream(headers: &mut http::HeaderMap) {
    headers.insert(
        ERROR_SOURCE_HEADER_NAME,
        http::HeaderValue::from_static(SOURCE_UPSTREAM),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[tokio::test]
    async fn gateway_errors_are_problem_json_with_the_oagw_type() {
        let response = OagwError::new(DomainError::RouteNotFound("nope".into()))
            .with_instance("/oagw/v1/proxy/nope")
            .into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/problem+json")
        );
        assert_eq!(
            response
                .headers()
                .get(ERROR_SOURCE_HEADER_NAME)
                .and_then(|value| value.to_str().ok()),
            Some(SOURCE_GATEWAY)
        );
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body");
        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(
            value["type"],
            "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
        );
        assert_eq!(value["status"], 404);
        assert_eq!(value["instance"], "/oagw/v1/proxy/nope");
        assert!(value["title"].is_string());
        assert!(value["detail"].is_string());
    }

    #[tokio::test]
    async fn a_rate_limit_failure_carries_the_retry_hint() {
        let response = OagwError::new(DomainError::RateLimitExceeded("exhausted".into()))
            .with_header("retry-after", "7")
            .into_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response
                .headers()
                .get(http::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok()),
            Some("7")
        );
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body");
        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(value["retry_after_seconds"], 7);
    }

    #[test]
    fn a_409_is_produced_for_a_plugin_in_use() {
        let response =
            OagwError::new(DomainError::PluginInUse("referenced".into())).into_response();
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }
}
