//! REST error surface: maps [`DomainError`] onto an RFC 9457 problem whose
//! `type` is the OAGW GTS error instance id (DESIGN §3.3).
//!
//! The body is assembled as a `toolkit_canonical_errors::Problem` so the
//! gateway's `canonical_error_middleware` can fill `instance` / `trace_id`
//! without dropping the OAGW extension fields, which live inside `context`.

use axum::http::header;
use axum::response::{IntoResponse, Response};
use toolkit_canonical_errors::Problem;

use crate::domain::error::DomainError;

/// Marker distinguishing gateway-produced errors from upstream-produced ones.
pub use crate::domain::error::ERROR_SOURCE_HEADER;
/// Value of [`ERROR_SOURCE_HEADER`] when OAGW itself generated the error.
pub use crate::domain::error::SOURCE_GATEWAY;
/// Value of [`ERROR_SOURCE_HEADER`] when the upstream produced the response.
pub use crate::domain::error::SOURCE_UPSTREAM;

/// A REST-ready error: a `problem+json` body plus the response headers.
#[derive(Debug, Clone)]
pub struct ApiError {
    /// The rendered problem document.
    pub problem: Problem,
    /// Extra response headers (`Retry-After`, rate-limit headers, …).
    pub headers: Vec<(String, String)>,
}

impl ApiError {
    /// Build an error from a domain error.
    #[must_use]
    pub fn new(err: &DomainError) -> Self {
        let problem = Problem {
            problem_type: gts_uri(err.gts_type()),
            title: err.title().to_owned(),
            status: err.status().as_u16(),
            detail: err.to_string(),
            instance: None,
            trace_id: None,
            context: err.context(),
            error_code: Some(err.code()),
            error_domain: Some(String::from("oagw.v1")),
        };
        let mut headers = vec![(
            String::from(ERROR_SOURCE_HEADER),
            String::from(SOURCE_GATEWAY),
        )];
        if let DomainError::RateLimitExceeded {
            retry_after_seconds,
            limit,
            remaining,
            reset_epoch_seconds,
        } = err
        {
            headers.push((String::from("retry-after"), retry_after_seconds.to_string()));
            headers.push((String::from("x-ratelimit-limit"), limit.to_string()));
            headers.push((String::from("x-ratelimit-remaining"), remaining.to_string()));
            headers.push((
                String::from("x-ratelimit-reset"),
                reset_epoch_seconds.to_string(),
            ));
        }
        Self { problem, headers }
    }

    /// Attach an extra response header.
    #[must_use]
    pub fn with_header(mut self, name: &str, value: String) -> Self {
        self.headers.push((name.to_owned(), value));
        self
    }
}

impl From<DomainError> for ApiError {
    fn from(err: DomainError) -> Self {
        Self::new(&err)
    }
}

impl From<&DomainError> for ApiError {
    fn from(err: &DomainError) -> Self {
        Self::new(err)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = serde_json::to_vec(&self.problem).unwrap_or_else(|_| {
            br#"{"type":"gts.cf.core.errors.err.v1~cf.oagw.internal.error.v1","title":"Internal error","status":500,"detail":"error serialization failed","context":null}"#.to_vec()
        });
        let status = axum::http::StatusCode::from_u16(self.problem.status)
            .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
        let mut response = (status, body).into_response();
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/problem+json"),
        );
        for (name, value) in self.headers {
            if let (Ok(name), Ok(value)) = (
                header::HeaderName::from_bytes(name.as_bytes()),
                header::HeaderValue::from_str(&value),
            ) {
                response.headers_mut().insert(name, value);
            }
        }
        response
    }
}

/// The `type` field is a GTS *URI* per RFC 9457 §3.1 as applied by the
/// platform (`gts://…`). OAGW's error ids are documented as bare GTS ids, so
/// the URI prefix is applied here and stripped again by clients that compare
/// against the DESIGN table.
fn gts_uri(gts_id: &str) -> String {
    if gts_id.starts_with("gts://") {
        gts_id.to_owned()
    } else {
        format!("gts://{gts_id}")
    }
}

/// Extract the bare GTS id from a problem `type` URI.
#[must_use]
pub fn bare_gts_id(problem_type: &str) -> &str {
    problem_type.strip_prefix("gts://").unwrap_or(problem_type)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_errors_map_to_the_design_gts_ids() {
        let err = ApiError::new(&DomainError::RouteNotFound);
        assert_eq!(err.problem.status, 404);
        assert_eq!(bare_gts_id(&err.problem.problem_type), crate::domain::gts_helpers::errors::ROUTE_NOT_FOUND);
        assert!(err.headers.iter().any(|(k, _)| k == ERROR_SOURCE_HEADER));
    }

    #[test]
    fn rate_limit_errors_carry_retry_after() {
        let err = ApiError::new(&DomainError::RateLimitExceeded {
            retry_after_seconds: 7,
            limit: 10,
            remaining: 0,
            reset_epoch_seconds: 100,
        });
        assert_eq!(err.problem.status, 429);
        let retry = err
            .headers
            .iter()
            .find(|(k, _)| k == "retry-after")
            .map(|(_, v)| v.as_str());
        assert_eq!(retry, Some("7"));
    }

    #[test]
    fn secrets_never_reach_the_wire() {
        let err = ApiError::new(&DomainError::AuthFailed(
            "upstream rejected the credentials".into(),
        ));
        let body = serde_json::to_string(&err.problem).expect("json");
        assert!(!body.contains("sk-"));
        assert_eq!(err.problem.context["reason"], "upstream rejected the credentials");
    }

    #[test]
    fn context_survives_a_problem_round_trip() {
        let err = ApiError::new(&DomainError::RequiredHeaderMissing("x-sig".into()));
        let rendered = serde_json::to_string(&err.problem).expect("json");
        let reparsed: Problem = serde_json::from_str(&rendered).expect("reparse");
        assert_eq!(reparsed.context["header"], "x-sig");
        assert_eq!(reparsed.status, err.problem.status);
    }
}
