//! RFC 9457 problem details for every gateway-produced error
//! (`contracts/errors.md`, `DESIGN.md` § 3.3, `ADR/0007`).
//!
//! The document is rendered here rather than through the platform's
//! [`toolkit_canonical_errors::Problem`] because the OAGW extension members —
//! `upstream_id`, `host`, `path`, `retry_after_seconds`, `valid_hosts` — are
//! **top-level** members of the problem document, not members of a nested
//! `context` object.

use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::domain::error::{DomainError, error_type_id};

/// `X-OAGW-Error-Source` value for a gateway-produced failure.
pub const ERROR_SOURCE_GATEWAY: &str = "gateway";

/// `X-OAGW-Error-Source` value for a failure the upstream produced.
pub const ERROR_SOURCE_UPSTREAM: &str = "upstream";

/// An RFC 9457 problem document with the gateway's extension members.
#[derive(Debug, Clone, Serialize)]
pub struct ProblemBody {
    /// URI reference identifying the error class.
    #[serde(rename = "type")]
    pub kind: String,
    /// Short, human-readable summary.
    pub title: String,
    /// HTTP status.
    pub status: u16,
    /// Human-readable explanation.
    pub detail: String,
    /// Request path, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    /// Correlation identifier, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// Upstream the request was bound to, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_id: Option<String>,
    /// Target host, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Upstream-bound path, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Seconds until the caller should retry, when the failure is transient.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u64>,
    /// Endpoints the `X-OAGW-Target-Host` header would have accepted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_hosts: Option<Vec<String>>,
    /// The rejected `X-OAGW-Target-Host` value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invalid_value: Option<String>,
}

/// The request details a problem document names in its extension members
/// (`contracts/errors.md` § "Specific extension fields").
#[derive(Debug, Clone, Default)]
pub struct RequestDetails {
    /// The upstream the request resolved to, when it did.
    pub upstream_id: Option<String>,
    /// The upstream's alias.
    pub host: Option<String>,
    /// The upstream-bound path.
    pub path: Option<String>,
    /// The correlation identifier the caller supplied, when it did.
    pub trace_id: Option<String>,
}

/// A [`DomainError`] rendered as a gateway problem document.
#[derive(Debug, Clone)]
pub struct OagwError {
    error: DomainError,
    context: Option<ErrorContext>,
    // Boxed: the details ride along only on the error path, and the type is
    // returned by value from every handler.
    details: Box<RequestDetails>,
    rate: Option<crate::infra::ratelimit::RateLimitDecision>,
}

/// Where a problem document came from, when it is not the gateway itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorContext {
    /// The upstream produced the failure.
    Upstream,
}

impl OagwError {
    /// Wraps a domain error as a gateway problem.
    #[must_use]
    pub fn gateway(error: DomainError) -> Self {
        Self {
            error,
            context: None,
            details: Box::default(),
            rate: None,
        }
    }

    /// Wraps a domain error with a specific source header.
    #[must_use]
    pub fn with_context(error: DomainError, context: ErrorContext) -> Self {
        Self {
            error,
            context: Some(context),
            details: Box::default(),
            rate: None,
        }
    }

    /// Names the request in the problem document's extension members.
    #[must_use]
    pub fn with_details(mut self, details: RequestDetails) -> Self {
        self.details = Box::new(details);
        self
    }

    /// The wrapped error.
    #[must_use]
    pub fn error(&self) -> &DomainError {
        &self.error
    }

    /// Attaches the rate-limit exposure a request earned before it was
    /// rejected by a later stage (`contracts/proxy-api.md` § 4).
    #[must_use]
    pub fn with_rate_limit(
        mut self,
        rate: Option<crate::infra::ratelimit::RateLimitDecision>,
    ) -> Self {
        self.rate = rate;
        self
    }

    fn source(&self) -> &'static str {
        match self.context {
            Some(ErrorContext::Upstream) => ERROR_SOURCE_UPSTREAM,
            None => ERROR_SOURCE_GATEWAY,
        }
    }
}

impl From<DomainError> for OagwError {
    fn from(error: DomainError) -> Self {
        Self::gateway(error)
    }
}

/// The status, problem body and headers of one gateway failure.
impl OagwError {
    /// The response status.
    #[must_use]
    pub fn status(&self) -> u16 {
        self.error.status()
    }

    /// The problem document.
    #[must_use]
    pub fn body(&self) -> ProblemBody {
        let mut body = ProblemBody {
            kind: error_type_id(&self.error),
            title: self.error.title().to_owned(),
            status: self.error.status(),
            detail: detail_of(&self.error),
            instance: None,
            trace_id: None,
            upstream_id: None,
            host: None,
            path: None,
            retry_after_seconds: None,
            valid_hosts: None,
            invalid_value: None,
        };
        let details = &*self.details;
        match &self.error {
            // The extensions the error table names are filled from what the
            // request actually carried, so a caller can tell which upstream it
            // was talking to (`contracts/errors.md`).
            DomainError::UnknownTargetHost { valid_hosts, .. }
            | DomainError::MissingTargetHost { valid_hosts } => {
                body.valid_hosts = Some(valid_hosts.clone());
                body.upstream_id = details.upstream_id.clone();
                body.host = details.host.clone();
                body.path = details.path.clone();
                body.trace_id = details.trace_id.clone();
            }
            DomainError::InvalidTargetHost { value } => {
                body.invalid_value = Some(value.clone());
                body.upstream_id = details.upstream_id.clone();
                body.host = details.host.clone();
                body.path = details.path.clone();
                body.trace_id = details.trace_id.clone();
            }
            DomainError::PayloadTooLarge(_) => {
                body.path = details.path.clone();
                body.trace_id = details.trace_id.clone();
            }
            DomainError::LinkUnavailable
            | DomainError::ConnectionTimeout
            | DomainError::RequestTimeout
            | DomainError::IdleTimeout => {
                body.host = details.host.clone();
                body.trace_id = details.trace_id.clone();
            }
            _ => {}
        }
        if let DomainError::RateLimitExceeded {
            retry_after_secs, ..
        } = self.error
        {
            body.retry_after_seconds = Some(retry_after_secs);
        }
        if matches!(
            self.error,
            DomainError::LinkUnavailable
                | DomainError::ConnectionTimeout
                | DomainError::RequestTimeout
                | DomainError::IdleTimeout
        ) {
            body.retry_after_seconds = Some(1);
        }
        body
    }

    /// The headers the failure carries: the source tag plus `Retry-After` and
    /// the rate-limit exposure headers when the failure is a 429.
    #[must_use]
    pub fn headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            crate::infra::proxy::headers::ERROR_SOURCE_HEADER,
            HeaderValue::from_static(self.source()),
        );
        // A request that passed the rate-limit stage carries its exposure even
        // when a later stage rejects it.
        let rejected_for_rate = matches!(self.error, DomainError::RateLimitExceeded { .. });
        if !rejected_for_rate && let Some(decision) = self.rate {
            crate::infra::proxy::headers::apply_rate_limit(&mut headers, &decision);
        }
        if let DomainError::RateLimitExceeded {
            limit,
            window_secs,
            retry_after_secs,
        } = self.error
        {
            if let Ok(value) = HeaderValue::from_str(&retry_after_secs.to_string()) {
                headers.insert(header::RETRY_AFTER, value);
            }
            for (name, value) in [
                ("x-ratelimit-limit", limit.to_string()),
                ("x-ratelimit-remaining", "0".to_owned()),
                (
                    "x-ratelimit-reset",
                    window_secs.max(retry_after_secs).to_string(),
                ),
            ] {
                if let (Ok(name), Ok(value)) = (
                    header::HeaderName::try_from(name),
                    HeaderValue::from_str(&value),
                ) {
                    headers.insert(name, value);
                }
            }
        }
        headers
    }
}

fn detail_of(error: &DomainError) -> String {
    match error {
        DomainError::Validation(_) | DomainError::Internal(_) => error.to_string(),
        DomainError::RateLimitExceeded {
            limit, window_secs, ..
        } => format!("rate limit of {limit} per {window_secs}s exceeded"),
        _ => error.to_string(),
    }
}

impl IntoResponse for OagwError {
    fn into_response(self) -> Response {
        let body = self.body();
        let status = axum::http::StatusCode::from_u16(body.status)
            .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
        let mut response = (status, axum::Json(body)).into_response();
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/problem+json"),
        );
        let headers = self.headers();
        for (name, value) in &headers {
            response.headers_mut().insert(name, value.clone());
        }
        response
    }
}

/// Convenience alias for handlers that return the gateway error type.
pub type ApiResult<T> = Result<T, OagwError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_route_not_found_is_a_gateway_problem() {
        let rendered = OagwError::gateway(DomainError::RouteNotFound);
        assert_eq!(rendered.status(), 404);
        assert_eq!(
            rendered.body().kind,
            "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
        );
        assert_eq!(
            rendered.headers().get("x-oagw-error-source").unwrap(),
            "gateway"
        );
    }

    #[test]
    fn an_unreachable_upstream_names_its_host_and_correlation_id() {
        let details = RequestDetails {
            upstream_id: Some("0b6c9e2a-0000-0000-0000-000000000001".to_owned()),
            host: Some("api.openai.com".to_owned()),
            path: Some("/v1/chat".to_owned()),
            trace_id: Some("req-1".to_owned()),
        };
        let rendered = OagwError::gateway(DomainError::LinkUnavailable).with_details(details);
        let body = rendered.body();
        assert_eq!(body.host.as_deref(), Some("api.openai.com"));
        assert_eq!(body.trace_id.as_deref(), Some("req-1"));
        assert_eq!(body.retry_after_seconds, Some(1));
        // `upstream_id` is not one of LinkUnavailable's documented members.
        assert!(body.upstream_id.is_none());

        let too_large =
            OagwError::gateway(DomainError::PayloadTooLarge(64)).with_details(RequestDetails {
                path: Some("/v1/upload".to_owned()),
                ..RequestDetails::default()
            });
        assert_eq!(too_large.body().path.as_deref(), Some("/v1/upload"));
        assert_eq!(too_large.body().host, None);

        let target = OagwError::gateway(DomainError::MissingTargetHost {
            valid_hosts: vec!["a.vendor.com".to_owned()],
        })
        .with_details(RequestDetails {
            upstream_id: Some("0b6c9e2a-0000-0000-0000-000000000001".to_owned()),
            host: Some("vendor.com".to_owned()),
            path: Some("/v1".to_owned()),
            trace_id: None,
        });
        let body = target.body();
        assert_eq!(body.valid_hosts, Some(vec!["a.vendor.com".to_owned()]));
        assert_eq!(
            body.upstream_id.as_deref(),
            Some("0b6c9e2a-0000-0000-0000-000000000001")
        );
        assert_eq!(body.host.as_deref(), Some("vendor.com"));
        assert_eq!(body.path.as_deref(), Some("/v1"));
    }

    #[test]
    fn a_rate_limit_carries_retry_after_and_the_source_header() {
        let rendered = OagwError::gateway(DomainError::RateLimitExceeded {
            limit: 5,
            window_secs: 1,
            retry_after_secs: 3,
        });
        let headers = rendered.headers();
        assert_eq!(headers.get(header::RETRY_AFTER).unwrap(), "3");
        assert_eq!(headers.get("x-oagw-error-source").unwrap(), "gateway");
        let body = rendered.body();
        assert_eq!(body.retry_after_seconds, Some(3));
    }

    #[test]
    fn target_host_failures_expose_the_valid_hosts() {
        let rendered = OagwError::gateway(DomainError::UnknownTargetHost {
            value: "ap.vendor.com".to_owned(),
            valid_hosts: vec!["us.vendor.com".to_owned()],
        });
        assert_eq!(
            rendered.body().valid_hosts.as_deref(),
            Some(["us.vendor.com".to_owned()].as_slice())
        );
    }

    #[test]
    fn an_upstream_sourced_error_is_tagged_upstream() {
        let rendered =
            OagwError::with_context(DomainError::DownstreamError, ErrorContext::Upstream);
        assert_eq!(
            rendered.headers().get("x-oagw-error-source").unwrap(),
            "upstream"
        );
    }
}
