//! REST error mapping for the OAGW gear (RFC 9457 problem details).
//!
//! Every gateway error is serialized as an `application/problem+json`
//! document carrying the GTS `type` id, the standard `title` / `status` /
//! `detail` / `instance` fields and the OAGW-specific extension members.
//!
//! ## Extension handling
//!
//! The toolkit canonical-error middleware (installed outermost on the gear
//! router) re-serializes every `application/problem+json` response through
//! its own [`toolkit_canonical_errors::Problem`] struct, which preserves
//! exactly one extension carrier. All OAGW extension members are therefore
//! nested under `context` so they survive that round trip intact.
//!
//! [`toolkit_canonical_errors::Problem`]:
//!     toolkit_canonical_errors::Problem

use axum::body::Body;
use axum::http::header::{CONTENT_TYPE, RETRY_AFTER};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::domain::error::{DomainError, ProblemContext};
use crate::domain::services::data_plane::{ERROR_SOURCE_GATEWAY, ERROR_SOURCE_HEADER};

/// `Content-Type` for RFC 9457 problem responses.
const PROBLEM_JSON: &str = "application/problem+json";

/// Wire problem body for OAGW gateway errors (RFC 9457).
#[derive(Debug, Clone, Serialize)]
pub struct OagwProblem {
    /// GTS error `type` id (e.g. `gts.cf.core.errors.err.v1~cf.oagw.*.v1`).
    #[serde(rename = "type")]
    pub type_id: String,
    /// Stable human-readable title.
    pub title: String,
    /// HTTP status code.
    pub status: u16,
    /// Occurrence-specific detail.
    pub detail: String,
    /// URI identifying this occurrence (the request path).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    /// Correlation id (filled by the canonical-error middleware when absent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// OAGW extension context.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<serde_json::Value>,
}

impl OagwProblem {
    /// Build a problem body from a [`DomainError`].
    #[must_use]
    pub fn from_domain_error(err: &DomainError, instance: Option<String>) -> Self {
        let info = err.problem_info();
        Self {
            type_id: info.type_id.to_owned(),
            title: info.title.to_owned(),
            status: info.status,
            detail: info.detail,
            instance,
            trace_id: None,
            context: info.context.as_ref().map(context_to_json),
        }
    }

    /// Convenience constructor for non-domain gateway errors (e.g. proxy
    /// handler wiring failures).
    #[must_use]
    pub fn gateway(status: u16, type_id: impl Into<String>, title: &str, detail: String) -> Self {
        Self {
            type_id: type_id.into(),
            title: title.to_owned(),
            status,
            detail,
            instance: None,
            trace_id: None,
            context: None,
        }
    }

    /// Attach the request path as the `instance` member.
    #[must_use]
    pub fn with_instance(mut self, instance: impl Into<String>) -> Self {
        self.instance = Some(instance.into());
        self
    }
}

impl From<(DomainError, Option<String>)> for OagwProblem {
    fn from((err, instance): (DomainError, Option<String>)) -> Self {
        Self::from_domain_error(&err, instance)
    }
}

impl IntoResponse for OagwProblem {
    fn into_response(self) -> Response {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static(PROBLEM_JSON));
        headers.insert(
            ERROR_SOURCE_HEADER,
            HeaderValue::from_static(ERROR_SOURCE_GATEWAY),
        );

        // Retry / rate-limit guidance pulled from the extension context so
        // the transport emits the standard headers on 429s.
        if let Some(context) = self.context.as_ref() {
            if let Some(secs) = context
                .get("retry_after_seconds")
                .and_then(serde_json::Value::as_u64)
                && let Ok(value) = HeaderValue::from_str(&secs.to_string())
            {
                headers.insert(RETRY_AFTER, value);
            }
            for (name, key) in [
                ("x-ratelimit-limit", "rate_limit_limit"),
                ("x-ratelimit-remaining", "rate_limit_remaining"),
                ("x-ratelimit-reset", "rate_limit_reset"),
            ] {
                if let Some(v) = context.get(key).and_then(serde_json::Value::as_u64)
                    && let Ok(value) = HeaderValue::from_str(&v.to_string())
                {
                    headers.insert(name, value);
                }
            }
        }

        let body = if let Ok(bytes) = serde_json::to_vec(&self) {
            bytes
        } else {
            let fallback = OagwProblem::gateway(
                500,
                crate::domain::gts_helpers::ERROR_DOWNSTREAM,
                "Internal Error",
                "failed to serialize problem response".to_owned(),
            );
            serde_json::to_vec(&fallback).unwrap_or_default()
        };

        let mut response = Response::new(Body::from(body));
        *response.status_mut() =
            StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        *response.headers_mut() = headers;
        response
    }
}

/// Serialize the domain [`ProblemContext`] into the wire `context` object.
fn context_to_json(context: &ProblemContext) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    if let Some(v) = &context.upstream_id {
        out.insert(
            "upstream_id".to_owned(),
            serde_json::Value::String(v.clone()),
        );
    }
    if let Some(v) = &context.alias {
        out.insert("alias".to_owned(), serde_json::Value::String(v.clone()));
    }
    if let Some(v) = &context.host {
        out.insert("host".to_owned(), serde_json::Value::String(v.clone()));
    }
    if let Some(v) = &context.path {
        out.insert("path".to_owned(), serde_json::Value::String(v.clone()));
    }
    if let Some(v) = context.retry_after_seconds {
        out.insert("retry_after_seconds".to_owned(), serde_json::Value::from(v));
    }
    if let Some(v) = context.rate_limit_limit {
        out.insert("rate_limit_limit".to_owned(), serde_json::Value::from(v));
    }
    if let Some(v) = context.rate_limit_remaining {
        out.insert(
            "rate_limit_remaining".to_owned(),
            serde_json::Value::from(v),
        );
    }
    if let Some(v) = context.rate_limit_reset {
        out.insert("rate_limit_reset".to_owned(), serde_json::Value::from(v));
    }
    if let Some(v) = &context.trace_id {
        out.insert("trace_id".to_owned(), serde_json::Value::String(v.clone()));
    }
    if let Some(v) = &context.plugin_id {
        out.insert("plugin_id".to_owned(), serde_json::Value::String(v.clone()));
    }
    if let Some(v) = &context.valid_hosts {
        out.insert(
            "valid_hosts".to_owned(),
            serde_json::Value::Array(v.iter().cloned().map(serde_json::Value::String).collect()),
        );
    }
    if let Some(v) = &context.invalid_value {
        out.insert(
            "invalid_value".to_owned(),
            serde_json::Value::String(v.clone()),
        );
    }
    if let Some(v) = &context.referenced_by {
        out.insert(
            "referenced_by".to_owned(),
            serde_json::to_value(v).unwrap_or_default(),
        );
    }
    serde_json::Value::Object(out)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn problem_carries_namespaced_extensions() {
        let ctx = ProblemContext {
            alias: Some("api.example.com".to_owned()),
            retry_after_seconds: Some(7),
            rate_limit_limit: Some(10),
            ..ProblemContext::new()
        };
        let err = DomainError::RateLimitExceeded {
            detail: "limited".into(),
            retry_after_seconds: 7,
            context: Some(ctx),
        };
        let problem = OagwProblem::from_domain_error(&err, Some("/oagw/v1/proxy/x".into()));
        let json = serde_json::to_value(&problem).unwrap();
        assert_eq!(
            json["type"],
            "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1"
        );
        assert_eq!(json["status"], 429);
        assert_eq!(json["instance"], "/oagw/v1/proxy/x");
        assert_eq!(json["context"]["retry_after_seconds"], 7);
        assert_eq!(json["context"]["rate_limit_limit"], 10);
        assert_eq!(json["context"]["alias"], "api.example.com");
    }

    #[test]
    fn problem_roundtrips_through_canonical_problem() {
        // The canonical middleware deserializes my body into its Problem
        // struct and re-serializes; extensions must survive via `context`.
        let err = DomainError::RateLimitExceeded {
            detail: "limited".into(),
            retry_after_seconds: 3,
            context: Some(ProblemContext::new()),
        };
        let problem = OagwProblem::from_domain_error(&err, None);
        let body = serde_json::to_vec(&problem).unwrap();
        let canonical: toolkit_canonical_errors::Problem = serde_json::from_slice(&body).unwrap();
        let reencoded = serde_json::to_string(&canonical).unwrap();
        let value: serde_json::Value = serde_json::from_str(&reencoded).unwrap();
        assert_eq!(
            value["type"],
            "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1"
        );
        assert_eq!(value["status"], 429);
    }

    #[test]
    fn into_response_sets_error_source_and_retry_headers() {
        let err = DomainError::RateLimitExceeded {
            detail: "limited".into(),
            retry_after_seconds: 5,
            context: Some(ProblemContext {
                rate_limit_limit: Some(100),
                rate_limit_remaining: Some(0),
                rate_limit_reset: Some(1_700_000_000),
                ..ProblemContext::new()
            }),
        };
        let resp = OagwProblem::from_domain_error(&err, None).into_response();
        assert_eq!(resp.headers()["x-oagw-error-source"], "gateway");
        assert_eq!(resp.headers()["retry-after"], "5");
        assert_eq!(resp.headers()["x-ratelimit-limit"], "100");
        assert_eq!(resp.headers()["x-ratelimit-remaining"], "0");
        assert_eq!(resp.headers()["x-ratelimit-reset"], "1700000000");
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(resp.headers()[CONTENT_TYPE], "application/problem+json");
    }
}
