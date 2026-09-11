//! RFC 9457 error rendering.
//!
//! Every OAGW error response is `application/problem+json` with a GTS `type`
//! identifier (`cpt-cf-oagw-principle-rfc9457`) and carries
//! `X-OAGW-Error-Source` (`ADR/0007-error-source-distinction.md`).
//!
//! # Why extension members appear twice
//!
//! The platform's canonical error middleware re-serializes any
//! `application/problem+json` response through
//! `toolkit_canonical_errors::Problem`, whose extension surface is the
//! `context` object; top-level members it does not know about are dropped on
//! that round-trip. OAGW's own contract (`docs/DESIGN.md` §3.3 and the
//! examples in ADR 0007) puts `upstream_id`, `valid_hosts` and friends at the
//! top level. Writing them in both places satisfies both contracts: direct
//! consumers read the documented top-level members, and anything that
//! survives the canonical round-trip still finds them under `context`.

use axum::response::{IntoResponse, Response};
use http::{HeaderName, HeaderValue, StatusCode, header};
use serde_json::{Map, Value, json};

use crate::domain::error::{DomainError, ErrorSource};
use crate::domain::services::proxy::ERROR_SOURCE_HEADER;

/// Media type for RFC 9457 problem documents.
pub const PROBLEM_JSON: &str = "application/problem+json";

/// Build the problem document for `err`.
#[must_use]
pub fn problem_body(err: &DomainError, instance: &str) -> Value {
    let mut body = Map::new();
    body.insert("type".to_owned(), json!(err.gts_type()));
    body.insert("title".to_owned(), json!(err.title()));
    body.insert("status".to_owned(), json!(err.status()));
    body.insert("detail".to_owned(), json!(err.detail()));
    body.insert("instance".to_owned(), json!(instance));

    let extensions = err.extensions().clone();
    for (key, value) in &extensions {
        body.insert(key.clone(), value.clone());
    }
    body.insert("context".to_owned(), Value::Object(extensions));

    Value::Object(body)
}

/// Render `err` as a full HTTP response.
#[must_use]
pub fn problem_response(err: &DomainError, instance: &str, source: ErrorSource) -> Response {
    let status = StatusCode::from_u16(err.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let body = problem_body(err, instance);

    let mut response = (status, axum::Json(body)).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(PROBLEM_JSON),
    );
    response.headers_mut().insert(
        HeaderName::from_static(ERROR_SOURCE_HEADER),
        HeaderValue::from_static(source.as_str()),
    );
    if let Some(seconds) = err.retry_after_seconds()
        && let Ok(value) = HeaderValue::from_str(&seconds.to_string())
    {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

impl IntoResponse for DomainError {
    fn into_response(self) -> Response {
        // Handlers that have a concrete request path use
        // [`problem_response`] directly; this fallback keeps `?` usable in
        // handlers where the path adds nothing.
        problem_response(&self, "", ErrorSource::Gateway)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::gts_helpers::errors;

    #[test]
    fn body_carries_the_rfc9457_members() {
        let err = DomainError::not_found("no route");
        let body = problem_body(&err, "/oagw/v1/proxy/api.example.com/x");
        assert_eq!(body["type"], json!(errors::ROUTE_NOT_FOUND));
        assert_eq!(body["title"], json!("Route Not Found"));
        assert_eq!(body["status"], json!(404));
        assert_eq!(body["detail"], json!("no route"));
        assert_eq!(body["instance"], json!("/oagw/v1/proxy/api.example.com/x"));
    }

    #[test]
    fn extensions_appear_at_the_top_level_and_under_context() {
        let err = DomainError::missing_target_host("pick one")
            .with_extension("valid_hosts", json!(["us.vendor.com", "eu.vendor.com"]))
            .with_extension("alias", "vendor.com");
        let body = problem_body(&err, "/oagw/v1/proxy/vendor.com/x");
        assert_eq!(body["alias"], json!("vendor.com"));
        assert_eq!(body["valid_hosts"][0], json!("us.vendor.com"));
        assert_eq!(body["context"]["alias"], json!("vendor.com"));
        assert_eq!(body["context"]["valid_hosts"][1], json!("eu.vendor.com"));
    }

    #[test]
    fn response_sets_content_type_and_error_source() {
        let response = problem_response(
            &DomainError::validation("bad"),
            "/oagw/v1/upstreams",
            ErrorSource::Gateway,
        );
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(response.headers()[header::CONTENT_TYPE], PROBLEM_JSON);
        assert_eq!(response.headers()[ERROR_SOURCE_HEADER], "gateway");
    }

    #[test]
    fn rate_limit_errors_carry_retry_after() {
        let err = DomainError::rate_limit_exceeded("slow down").with_retry_after(15);
        let response = problem_response(&err, "/oagw/v1/proxy/x", ErrorSource::Gateway);
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[header::RETRY_AFTER], "15");
    }

    #[test]
    fn an_unmapped_status_degrades_to_500() {
        let err = DomainError::new(1_099, errors::INTERNAL, "Weird", "out of range");
        let response = problem_response(&err, "/x", ErrorSource::Gateway);
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
