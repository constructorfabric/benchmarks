//! RFC 9457 problem responses (DESIGN §3.3 "Error Response Format").
//!
//! Every gateway error — management or proxy — is rendered here, so the status
//! code, the GTS `type` and the `X-OAGW-Error-Source` header can only change
//! together.

use axum::body::Body;
use http::{HeaderValue, StatusCode};
use serde_json::{Map, Value, json};

use crate::domain::error::DomainError;
use crate::domain::gts_helpers as gts;

/// The `application/problem+json` body for a gateway error.
#[must_use]
pub fn problem_body(error: &DomainError, instance: Option<&str>) -> Value {
    let mut body = Map::new();
    body.insert(
        "type".to_owned(),
        json!(gts::error_type(error.instance_id())),
    );
    body.insert("title".to_owned(), json!(error.title()));
    body.insert("status".to_owned(), json!(error.status().as_u16()));
    body.insert("detail".to_owned(), json!(error.to_string()));
    if let Some(instance) = instance {
        body.insert("instance".to_owned(), json!(instance));
    }
    if let Some(retry_after) = error.retry_after() {
        body.insert(
            "retry_after_seconds".to_owned(),
            json!(retry_after.as_secs()),
        );
    }
    body.insert("retriable".to_owned(), json!(error.retriable()));
    for (name, value) in error.members() {
        body.insert(name.to_owned(), value);
    }
    Value::Object(body)
}

/// An `application/problem+json` response for a gateway error.
#[must_use]
pub fn problem_response(error: &DomainError, instance: Option<&str>) -> http::Response<Body> {
    // Built by hand rather than through the fallible builder: a fixed content
    // type and well-known header names cannot fail to construct.
    let mut response = http::Response::new(Body::from(
        serde_json::to_vec(&problem_body(error, instance))
            .unwrap_or_else(|_| format!("{{\"status\":{}}}", error.status().as_u16()).into_bytes()),
    ));
    *response.status_mut() = error.status();
    let headers = response.headers_mut();
    headers.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/problem+json"),
    );
    headers.insert(
        http::HeaderName::from_static(gts::HEADER_ERROR_SOURCE),
        HeaderValue::from_static(gts::ERROR_SOURCE_GATEWAY),
    );
    if let Some(retry_after) = error.retry_after() {
        headers.insert(
            http::header::RETRY_AFTER,
            HeaderValue::from(retry_after.as_secs()),
        );
    }
    response
}

/// A management handler error, mapped to its problem response.
#[must_use]
pub fn management_error(error: &DomainError, path: &str) -> http::Response<Body> {
    problem_response(error, Some(path))
}

/// `501 Not Implemented` — the documented non-goals that are routed but not
/// implemented.
#[must_use]
pub fn not_implemented(feature: &str) -> http::Response<Body> {
    let mut response = problem_response(
        &DomainError::Validation(format!("{feature} is not implemented")),
        None,
    );
    *response.status_mut() = StatusCode::NOT_IMPLEMENTED;
    response
}
