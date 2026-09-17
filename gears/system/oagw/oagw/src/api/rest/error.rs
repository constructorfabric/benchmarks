//! Error → response mapping.
//!
//! Every `oagw` failure is rendered as a canonical RFC 9457
//! `application/problem+json` body carrying the contract GTS type id and
//! `X-OAGW-Error-Source: gateway` (ADR-0007). Upstream errors never pass
//! through here: the data plane returns those verbatim and stamps the header
//! with `upstream`.
//!
//! The body is a [`Problem`], not a hand-rolled map: the host's canonical
//! error middleware deserializes every problem body into that type to fill
//! `trace_id` / `instance` and log the failure, so `context` is where the
//! oagw extension members (`upstream_id`, `host`, `path`, `alias`,
//! `invalid_value`, `valid_hosts`, `retry_after_seconds`, `rate_limit`)
//! travel.

use axum::body::Body;
use axum::http::{HeaderValue, Request, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use toolkit_canonical_errors::Problem;

use crate::domain::error::DomainError;

/// Content type of an RFC 9457 problem document.
pub const PROBLEM_JSON: &str = "application/problem+json";

/// Gateway-originated error marker (ADR-0007).
pub const ERROR_SOURCE_GATEWAY: &str = "gateway";

/// Upstream-originated marker: a response the upstream produced.
pub const ERROR_SOURCE_UPSTREAM: &str = "upstream";

/// Header added to every gateway error (and every successful proxied
/// response) so clients can tell the two failure origins apart.
pub const ERROR_SOURCE_HEADER: &str = "X-OAGW-Error-Source";

/// A [`DomainError`] turned into an axum response.
#[derive(Debug)]
pub struct ProblemResponse {
    error: DomainError,
    instance: Option<String>,
}

impl ProblemResponse {
    /// Bind this problem to the request path it occurred on.
    #[must_use]
    pub fn instance(mut self, instance: impl Into<String>) -> Self {
        self.instance = Some(instance.into());
        self
    }
}

impl From<DomainError> for ProblemResponse {
    fn from(error: DomainError) -> Self {
        Self {
            error,
            instance: None,
        }
    }
}

impl IntoResponse for ProblemResponse {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.error.kind.http_status())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let extensions = self.error.extensions;

        // The oagw extension members live in the problem document's
        // `context`: the host's canonical error middleware deserializes every
        // `application/problem+json` body into a canonical `Problem` and
        // re-serializes it, so anything outside the canonical field set
        // would be dropped from the wire (DESIGN.md §3.3 names them as
        // top-level extension fields; `context` is where the canonical
        // contract carries per-domain members).
        let mut context = serde_json::Map::new();
        for (key, value) in [
            ("upstream_id", &extensions.upstream_id),
            ("host", &extensions.host),
            ("path", &extensions.path),
            ("alias", &extensions.alias),
            ("invalid_value", &extensions.invalid_value),
        ] {
            if let Some(value) = value {
                context.insert(key.to_owned(), json!(value));
            }
        }
        if let Some(seconds) = extensions.retry_after_seconds {
            context.insert(String::from("retry_after_seconds"), json!(seconds));
        }
        if let Some(budget) = extensions.rate_limit {
            context.insert(
                String::from("rate_limit"),
                json!({
                    "limit": budget.limit,
                    "remaining": budget.remaining,
                    "reset_at": budget.reset_at
                }),
            );
        }
        if !extensions.valid_hosts.is_empty() {
            context.insert(String::from("valid_hosts"), json!(extensions.valid_hosts));
        }

        let problem = Problem {
            problem_type: self.error.kind.gts_type(),
            title: self.error.kind.title().to_owned(),
            status: status.as_u16(),
            detail: self.error.detail.clone(),
            instance: self.instance.or_else(|| extensions.path.clone()),
            trace_id: extensions.trace_id.clone(),
            context: Value::Object(context),
            error_code: None,
            error_domain: None,
        };

        let mut response = problem.into_response();
        response.headers_mut().insert(
            header::HeaderName::from_static("x-oagw-error-source"),
            HeaderValue::from_static(ERROR_SOURCE_GATEWAY),
        );
        if let Some(value) = extensions
            .retry_after_seconds
            .and_then(|seconds| HeaderValue::from_str(&seconds.to_string()).ok())
        {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        if let Some(budget) = extensions.rate_limit {
            for (name, value) in [
                ("x-ratelimit-limit", budget.limit.to_string()),
                ("x-ratelimit-remaining", budget.remaining.to_string()),
                ("x-ratelimit-reset", budget.reset_at.to_string()),
            ] {
                if let Ok(value) = HeaderValue::from_str(&value) {
                    response.headers_mut().insert(name, value);
                }
            }
        }
        response
    }
}

/// Handler result alias: `?` converts a [`DomainError`] into a problem
/// document.
pub type ApiResult<T> = Result<T, ProblemResponse>;

/// Stamp `instance` on outgoing problem documents without each handler having
/// to thread the request path through.
///
/// Only `application/problem+json` bodies are buffered; proxied responses are
/// streaming and carry an upstream content type.
pub async fn set_problem_instance(request: Request<Body>, next: Next) -> Response {
    let instance = request.uri().path().to_owned();
    let mut response = next.run(request).await;
    let is_problem = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with(PROBLEM_JSON));
    if !is_problem {
        return response;
    }
    let Ok(bytes) = axum::body::to_bytes(std::mem::take(response.body_mut()), 1 << 20).await else {
        return response;
    };
    match serde_json::from_slice::<serde_json::Value>(&bytes) {
        Ok(mut body) if body.get("instance").is_none() => {
            body["instance"] = json!(instance);
            *response.body_mut() =
                Body::from(serde_json::to_vec(&body).unwrap_or_else(|_| bytes.to_vec()));
        }
        Ok(_) => {}
        Err(_) => {
            *response.body_mut() = Body::from(bytes);
        }
    }
    response
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod error_tests;
