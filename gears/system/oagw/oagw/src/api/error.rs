//! OAGW error projection (DESIGN §3.3 "Error Response Format", ADR-0007).
//!
//! Gateway errors are RFC 9457 problem documents carrying the OAGW GTS `type`
//! identifiers from the error catalog plus `X-OAGW-Error-Source: gateway`. The
//! projection is owned here rather than by `toolkit-canonical-errors` because
//! every row of the OAGW catalog carries its own GTS identifier, which the
//! canonical categories cannot express.

use axum::Json;
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::domain::error::DomainError;

/// Response header naming where an error originated.
pub const ERROR_SOURCE_HEADER: &str = "X-OAGW-Error-Source";
/// Value of `X-OAGW-Error-Source` for errors the gateway itself produced.
pub const ERROR_SOURCE_GATEWAY: &str = "gateway";
/// Value of `X-OAGW-Error-Source` for errors relayed from the upstream.
pub const ERROR_SOURCE_UPSTREAM: &str = "upstream";

/// `application/problem+json` content type.
const PROBLEM_CONTENT_TYPE: &str = "application/problem+json";

/// RFC 9457 problem document with the OAGW extension fields.
#[derive(Debug, Serialize)]
pub struct OagwProblem {
    /// GTS identifier of the error type.
    #[serde(rename = "type")]
    pub problem_type: String,
    /// Human-readable summary.
    pub title: String,
    /// HTTP status code.
    pub status: u16,
    /// Human-readable explanation of this occurrence.
    pub detail: String,
    /// Retry guidance, present on retriable errors.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u64>,
    /// Tracing correlation identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// Rate-limit limit value, present on 429 responses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    /// Remaining tokens, present on 429 responses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining: Option<u64>,
    /// Rate-limit window reset, present on 429 responses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_seconds: Option<u64>,
}

/// An OAGW error rendered onto the wire.
#[derive(Debug)]
pub struct OagwError {
    problem: Box<OagwProblem>,
    headers: Vec<(String, String)>,
}

impl OagwError {
    /// Builds the gateway-sourced problem document for a domain error.
    #[must_use]
    pub fn from_domain(error: &DomainError) -> Self {
        let mut headers = Vec::new();
        if let DomainError::RateLimitExceeded {
            limit,
            remaining,
            reset_seconds,
            retry_after_seconds,
        } = error
        {
            headers.push(("Retry-After".to_owned(), retry_after_seconds.to_string()));
            headers.push(("X-RateLimit-Limit".to_owned(), limit.to_string()));
            headers.push(("X-RateLimit-Remaining".to_owned(), remaining.to_string()));
            headers.push(("X-RateLimit-Reset".to_owned(), reset_seconds.to_string()));
        } else if let Some(retry_after) = error.retry_after_seconds() {
            headers.push(("Retry-After".to_owned(), retry_after.to_string()));
        }
        // ADR-0007: a failure the upstream produced is attributed to it, even
        // when the gateway renders the problem document itself.
        if error.is_upstream_failure() {
            headers.push((
                ERROR_SOURCE_HEADER.to_owned(),
                ERROR_SOURCE_UPSTREAM.to_owned(),
            ));
        }
        Self {
            problem: Box::new(OagwProblem {
                problem_type: error.gts_type().to_owned(),
                title: error.error_type().to_owned(),
                status: error.status_code(),
                detail: error.to_string(),
                retry_after_seconds: error.retry_after_seconds(),
                trace_id: None,
                limit: match error {
                    DomainError::RateLimitExceeded { limit, .. } => Some(*limit),
                    _ => None,
                },
                remaining: match error {
                    DomainError::RateLimitExceeded { remaining, .. } => Some(*remaining),
                    _ => None,
                },
                reset_seconds: match error {
                    DomainError::RateLimitExceeded { reset_seconds, .. } => Some(*reset_seconds),
                    _ => None,
                },
            }),
            headers,
        }
    }

    /// Attaches a tracing identifier to the problem document.
    #[must_use]
    pub fn with_trace_id(mut self, trace_id: impl Into<String>) -> Self {
        self.problem.trace_id = Some(trace_id.into());
        self
    }

    /// Adds a response header to the error response.
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// The HTTP status of the error response.
    #[must_use]
    pub const fn status(&self) -> u16 {
        self.problem.status
    }

    /// The rendered problem document.
    #[must_use]
    pub const fn problem(&self) -> &OagwProblem {
        &self.problem
    }
}

impl From<DomainError> for OagwError {
    fn from(error: DomainError) -> Self {
        Self::from_domain(&error)
    }
}

impl From<&DomainError> for OagwError {
    fn from(error: &DomainError) -> Self {
        Self::from_domain(error)
    }
}

impl IntoResponse for OagwError {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.problem.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut response = (status, Json(self.problem)).into_response();
        if let Ok(value) = HeaderValue::from_str(ERROR_SOURCE_GATEWAY) {
            response
                .headers_mut()
                .insert(HeaderName::from_static("x-oagw-error-source"), value);
        }
        for (name, value) in self.headers {
            let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else {
                continue;
            };
            if let Ok(value) = HeaderValue::from_str(&value) {
                response.headers_mut().insert(name, value);
            }
        }
        response.headers_mut().insert(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static(PROBLEM_CONTENT_TYPE),
        );
        response
    }
}

/// Renders an upstream failure as a gateway error with the upstream headers
/// preserved (`X-OAGW-Error-Source: upstream`, ADR-0007).
#[must_use]
pub fn upstream_error(status: u16, body: String) -> Response {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut response = (status, body).into_response();
    if let Ok(value) = HeaderValue::from_str(ERROR_SOURCE_UPSTREAM) {
        response
            .headers_mut()
            .insert(HeaderName::from_static("x-oagw-error-source"), value);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::error::codes;

    #[test]
    fn every_error_maps_to_its_catalog_row() {
        let cases: Vec<(DomainError, u16, &str)> = vec![
            (DomainError::validation("bad"), 400, codes::VALIDATION),
            (
                DomainError::RouteError("bad".to_owned()),
                400,
                codes::VALIDATION,
            ),
            (
                DomainError::RouteRejected("bad".to_owned()),
                400,
                codes::ROUTE_REJECTED,
            ),
            (
                DomainError::MissingTargetHost {
                    alias: "api".to_owned(),
                },
                400,
                codes::MISSING_TARGET_HOST,
            ),
            (
                DomainError::InvalidTargetHost {
                    value: "!!".to_owned(),
                },
                400,
                codes::INVALID_TARGET_HOST,
            ),
            (
                DomainError::UnknownTargetHost {
                    value: "x".to_owned(),
                },
                400,
                codes::UNKNOWN_TARGET_HOST,
            ),
            (
                DomainError::AuthenticationFailed("no".to_owned()),
                401,
                codes::AUTH_FAILED,
            ),
            (
                DomainError::RouteNotFound("x".to_owned()),
                404,
                codes::ROUTE_NOT_FOUND,
            ),
            (
                DomainError::NotFound {
                    resource: "upstream",
                    id: "1".to_owned(),
                },
                404,
                codes::RESOURCE_NOT_FOUND,
            ),
            (
                DomainError::AliasConflict {
                    alias: "api".to_owned(),
                },
                409,
                codes::ALIAS_CONFLICT,
            ),
            (
                DomainError::PluginInUse {
                    plugin_id: "1".to_owned(),
                    upstreams: vec![],
                    routes: vec![],
                },
                409,
                codes::PLUGIN_IN_USE,
            ),
            (
                DomainError::ConcurrentModification("x".to_owned()),
                409,
                codes::CONCURRENT_MODIFICATION,
            ),
            (
                DomainError::PayloadTooLarge { limit: 1 },
                413,
                codes::PAYLOAD_TOO_LARGE,
            ),
            (
                DomainError::RateLimitExceeded {
                    limit: 10,
                    remaining: 0,
                    reset_seconds: 30,
                    retry_after_seconds: 30,
                },
                429,
                codes::RATE_LIMIT_EXCEEDED,
            ),
            (
                DomainError::SecretNotFound("cred://x".to_owned()),
                500,
                codes::SECRET_NOT_FOUND,
            ),
            (
                DomainError::ProtocolError("x".to_owned()),
                502,
                codes::PROTOCOL_ERROR,
            ),
            (
                DomainError::DownstreamError("x".to_owned()),
                502,
                codes::DOWNSTREAM_ERROR,
            ),
            (
                DomainError::StreamAborted("x".to_owned()),
                502,
                codes::STREAM_ABORTED,
            ),
            (
                DomainError::LinkUnavailable("x".to_owned()),
                503,
                codes::LINK_UNAVAILABLE,
            ),
            (
                DomainError::CircuitBreakerOpen("x".to_owned()),
                503,
                codes::CIRCUIT_BREAKER_OPEN,
            ),
            (
                DomainError::PluginNotFound("x".to_owned()),
                503,
                codes::PLUGIN_NOT_FOUND,
            ),
            (
                DomainError::ConnectionTimeout,
                504,
                codes::CONNECTION_TIMEOUT,
            ),
            (DomainError::RequestTimeout, 504, codes::REQUEST_TIMEOUT),
            (DomainError::IdleTimeout, 504, codes::IDLE_TIMEOUT),
        ];
        for (error, status, problem_type) in cases {
            let rendered = OagwError::from_domain(&error);
            assert_eq!(rendered.status(), status, "{problem_type} status");
            assert_eq!(rendered.problem().problem_type, problem_type);
            assert_eq!(rendered.problem().status, status);
            assert!(!rendered.problem().title.is_empty());
        }
    }

    #[test]
    fn rate_limit_errors_carry_the_headers() {
        let error = OagwError::from_domain(&DomainError::RateLimitExceeded {
            limit: 100,
            remaining: 0,
            reset_seconds: 42,
            retry_after_seconds: 42,
        });
        let headers = &error.headers;
        assert!(headers.contains(&("Retry-After".to_owned(), "42".to_owned())));
        assert!(headers.contains(&("X-RateLimit-Limit".to_owned(), "100".to_owned())));
        assert!(headers.contains(&("X-RateLimit-Remaining".to_owned(), "0".to_owned())));
        assert_eq!(error.problem().retry_after_seconds, Some(42));
    }

    #[test]
    fn retriable_errors_carry_retry_after() {
        let error = OagwError::from_domain(&DomainError::LinkUnavailable("x".to_owned()));
        assert_eq!(error.problem().retry_after_seconds, Some(1));
        let non_retriable = OagwError::from_domain(&DomainError::validation("x"));
        assert_eq!(non_retriable.problem().retry_after_seconds, None);
    }

    #[test]
    fn trace_id_is_attached() {
        let error = OagwError::from_domain(&DomainError::validation("x")).with_trace_id("trace-1");
        assert_eq!(error.problem().trace_id.as_deref(), Some("trace-1"));
    }

    #[test]
    fn extra_headers_survive_rendering() {
        let error = OagwError::from_domain(&DomainError::validation("x"))
            .with_header("X-OAGW-Upstream-Id", "abc");
        assert!(
            error
                .headers
                .contains(&("X-OAGW-Upstream-Id".to_owned(), "abc".to_owned()))
        );
    }
}
