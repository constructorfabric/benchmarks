//! `DomainError` → RFC 9457 `application/problem+json` mapping.
//!
//! The OAGW error contract (DESIGN §3.3 "Error Response Format", ADR 0007) is
//! stricter than the platform's canonical error catalogue:
//!
//! * `type` carries the gear-specific GTS id `cf.oagw.*`, not the generic
//!   `cf.toolkit.*` ids.
//! * extension members (`upstream_id`, `valid_hosts`, `referenced_by`, …) are
//!   *flattened* into the problem document instead of being nested under a
//!   `context` member.
//! * every response carries `X-OAGW-Error-Source: gateway` so clients can tell
//!   gateway-generated problems from upstream passthrough bodies.
//!
//! Review evidence (privilege boundary — error surface):
//! * Guardrail: ADR 0007 "Error Source Distinction" + DESIGN §3.3
//!   "Standard Fields" / "Extension Fields".
//! * Rationale: a gateway error must never be mistaken for an upstream
//!   response; the header plus the `application/problem+json` media type make
//!   the distinction explicit even when intermediaries strip the header.
//! * Validation performed: `error_response_tests` asserts the media type, the
//!   `X-OAGW-Error-Source` value, the exact GTS `type`, the flattened
//!   extension members and the `Retry-After` header for retriable errors.

use axum::http::HeaderValue;
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value};

use crate::domain::error::DomainError;

/// Media type mandated by RFC 9457 and ADR 0007.
pub const PROBLEM_JSON: &str = "application/problem+json";
/// Header distinguishing gateway errors from upstream passthrough errors.
pub const ERROR_SOURCE_HEADER: &str = "x-oagw-error-source";
/// Value of [`ERROR_SOURCE_HEADER`] for errors produced by the gateway.
pub const ERROR_SOURCE_GATEWAY: &str = "gateway";
/// Value of [`ERROR_SOURCE_HEADER`] for errors passed through from upstream.
pub const ERROR_SOURCE_UPSTREAM: &str = "upstream";

/// Renders a problem document for `error`, addressed at `instance`.
#[must_use]
pub fn problem_document(error: &DomainError, instance: Option<&str>, trace_id: Option<&str>) -> Value {
    let mut body = Map::new();
    body.insert("type".to_owned(), Value::from(error.gts_type()));
    body.insert("title".to_owned(), Value::from(error.title()));
    body.insert("status".to_owned(), Value::from(error.status()));
    body.insert("detail".to_owned(), Value::from(error.detail()));
    body.insert(
        "instance".to_owned(),
        Value::from(instance.map_or_else(String::new, str::to_owned)),
    );

    let mut extensions = error.extensions();
    extensions.trace_id = trace_id.map(str::to_owned);
    for (member, value) in extensions.iter_json() {
        if !value.is_null() {
            body.insert(member, value);
        }
    }
    Value::Object(body)
}

/// Converts a [`DomainError`] into an axum response.
#[must_use]
pub fn into_response(error: &DomainError, instance: Option<&str>, trace_id: Option<&str>) -> Response {
    let status = axum::http::StatusCode::from_u16(error.status())
        .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    let body = problem_document(error, instance, trace_id);
    let payload = serde_json::to_string(&body)
        .unwrap_or_else(|_| "{\"title\":\"Internal Error\",\"status\":500}".to_owned());

    let mut response = (status, payload).into_response();
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static(PROBLEM_JSON),
    );
    response.headers_mut().insert(
        axum::http::HeaderName::from_static(ERROR_SOURCE_HEADER),
        HeaderValue::from_static(ERROR_SOURCE_GATEWAY),
    );
    if let Some(retry_after) = error.retry_after_seconds()
        && let Ok(value) = HeaderValue::from_str(&retry_after.to_string())
    {
        response
            .headers_mut()
            .insert(axum::http::header::RETRY_AFTER, value);
    }
    response
}

impl IntoResponse for DomainError {
    fn into_response(self) -> Response {
        into_response(&self, None, None)
    }
}

impl From<DomainError> for Response {
    fn from(error: DomainError) -> Response {
        error.into_response()
    }
}
