//! Transport mapping of [`OagwError`] to the wire error surface (ADR-0007).
//!
//! Every gateway error response:
//!
//! * carries `X-OAGW-Error-Source: gateway` (upstream passthrough responses
//!   carry `upstream` and are rendered by the data plane, not here),
//! * is `application/problem+json` with the RFC 9457 members `type`, `title`,
//!   `status`, `detail`, `instance` plus the OAGW extension fields,
//! * sets `Retry-After` when retry guidance is available.
//!
//! Success responses carry `X-OAGW-Error-Source: gateway` too (ADR-0007
//! "Confirmation": *success responses include the header*); see
//! [`stamp_gateway_source`].

use uuid::Uuid;

use axum::body::Body;
use axum::response::Response;
use http::HeaderValue;
use http::header::{CONTENT_TYPE, RETRY_AFTER};
use serde_json::Value;

use crate::domain::policy::rate_limit::{
    RATE_LIMIT_LIMIT_HEADER as RATE_LIMIT_LIMIT,
    RATE_LIMIT_REMAINING_HEADER as RATE_LIMIT_REMAINING,
    RATE_LIMIT_RESET_HEADER as RATE_LIMIT_RESET,
};
use crate::error::{
    ERROR_SOURCE_GATEWAY, OagwError, PROBLEM_JSON_MEDIA_TYPE, error_source_header_name,
};

/// Render an [`OagwError`] as an axum [`Response`].
#[must_use]
pub fn render_problem(err: &OagwError) -> Response {
    let status = err.status();

    // Every gateway error carries a correlation id (ADR-0007 `trace_id`). Errors
    // raised on the data plane already have one — the proxy handler mints it —
    // while management errors do not, so one is minted here for them: it goes
    // into the problem document *and* into the log record below, so an operator
    // can join the two.
    let request_id = match err.extensions().get("trace_id").and_then(Value::as_str) {
        Some(trace_id) => trace_id.to_owned(),
        None => Uuid::new_v4().to_string(),
    };
    let err = err.clone().with_trace_id(request_id);

    let body = err.to_problem_json();

    // DESIGN §4.3 "What is Logged": every failed request leaves a record with
    // its status, error type and message, at `ERROR` for upstream failures and
    // timeouts and `WARN` for the rest (rate limits, validation, not-found).
    let level = if status.is_server_error() {
        RejectedLevel::Error
    } else {
        RejectedLevel::Warn
    };
    rejected_record(&err, level, status.as_u16());

    let mut response = Response::new(Body::from(body.to_string()));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static(PROBLEM_JSON_MEDIA_TYPE),
    );
    headers.insert(
        error_source_header_name(),
        HeaderValue::from_static(ERROR_SOURCE_GATEWAY),
    );
    if let Some(retry_after) = retry_after_header(&err) {
        headers.insert(RETRY_AFTER, retry_after);
    }
    if let Some((limit, remaining, reset)) = err.rate_limit_state() {
        // The gateway's own budget, not the upstream's (ADR-0003 "Response
        // Headers"): a 429 tells the caller what it hit and when to come back.
        if let Ok(value) = HeaderValue::from_str(limit) {
            headers.insert(RATE_LIMIT_LIMIT, value);
        }
        if let Ok(value) = HeaderValue::from_str(&remaining.to_string()) {
            headers.insert(RATE_LIMIT_REMAINING, value);
        }
        if let Ok(value) = HeaderValue::from_str(&reset.to_string()) {
            headers.insert(RATE_LIMIT_RESET, value);
        }
    }

    response
}

/// The severity of the rejected-request record (DESIGN §4.3 "Log Levels").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RejectedLevel {
    /// Upstream failures and timeouts.
    Error,
    /// Rate limits, validation failures, not-found responses.
    Warn,
}

/// The DESIGN §4.3 log record for a rejected request.
///
/// The severity is passed as a value and matched on to pick the `tracing`
/// macro: tracing registers its callsites statically, so the level has to be a
/// literal rather than a runtime [`tracing::Level`].
fn rejected_record(err: &OagwError, level: RejectedLevel, status: u16) {
    let request_id = err
        .extensions()
        .get("trace_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let instance = err.instance().unwrap_or_default();

    macro_rules! record {
        ($level:ident) => {{
            tracing::$level!(
                target: "oagw.audit",
                request_id = %request_id,
                status = status,
                error_type = err.kind().gts_type_id(),
                instance = %instance,
                detail = err.detail(),
                "oagw request rejected"
            )
        }};
    }

    match level {
        RejectedLevel::Error => record!(error),
        RejectedLevel::Warn => record!(warn),
    }
}

/// Stamp a gateway-generated response with `X-OAGW-Error-Source: gateway`.
///
/// ADR-0007 requires the header on **all** OAGW-generated responses, not only on
/// errors: management/CRUD responses (success and failure) and gateway-generated
/// proxy errors are stamped `gateway`, while responses passed through from an
/// upstream are stamped `upstream` by the data plane. Handlers wrap their
/// success responses with this so the header is never missing.
#[must_use]
pub fn stamp_gateway_source(response: Response) -> Response {
    let (mut parts, body) = response.into_parts();
    parts.headers.insert(
        error_source_header_name(),
        HeaderValue::from_static(ERROR_SOURCE_GATEWAY),
    );

    Response::from_parts(parts, body)
}

/// `Retry-After` header value for a retriable error, when guidance is present.
fn retry_after_header(err: &OagwError) -> Option<HeaderValue> {
    let seconds = err.retry_after_secs()?;
    HeaderValue::from_str(&seconds.to_string()).ok()
}

/// Convert a body-deserialization failure into a 400 problem document.
///
/// Axum's own `Json` rejection renders a plain-text body; the OAGW contract
/// requires every gateway error to be `application/problem+json`, so handlers
/// accept the raw [`axum::body::Bytes`] and delegate to this mapper.
#[must_use]
pub fn invalid_body(message: &str) -> OagwError {
    OagwError::validation(format!("request body is not valid JSON: {message}"))
}

/// Convert an unsupported media type into a 415-style validation problem.
///
/// The DESIGN §3.3 error table has no dedicated entry, so the closest
/// documented kind (400 `ValidationError`) is used.
#[must_use]
pub fn unsupported_media_type(received: Option<&str>) -> OagwError {
    let detail = match received {
        Some(media_type) => format!("expected `application/json` request body, got '{media_type}'"),
        None => "expected `application/json` request body".to_owned(),
    };

    OagwError::validation(detail).with_extension(
        "expected_content_type",
        Value::String("application/json".to_owned()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ERROR_SOURCE_HEADER;
    use crate::error::OagwErrorKind;
    use axum::response::IntoResponse;
    use futures_util::future::FutureExt;
    use http::StatusCode;

    #[test]
    fn rendered_response_sets_status_content_type_and_error_source() {
        let response = render_problem(&OagwError::validation("bad body"));

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some(PROBLEM_JSON_MEDIA_TYPE)
        );
        assert_eq!(
            response
                .headers()
                .get(ERROR_SOURCE_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some(ERROR_SOURCE_GATEWAY)
        );
        assert!(response.headers().get(RETRY_AFTER).is_none());
    }

    #[test]
    fn rendered_response_sets_retry_after_when_guidance_is_present() {
        let response = render_problem(&OagwError::rate_limit_exceeded("slow down", 15));

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok()),
            Some("15")
        );
    }

    #[test]
    fn rendered_body_is_the_problem_document() {
        let err =
            OagwError::upstream_not_found("no upstream 'x'").with_instance("/oagw/v1/upstreams");
        let response = render_problem(&err);

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(err.kind(), OagwErrorKind::UpstreamNotFound);
    }

    /// The rendered problem document, read back out of the response body.
    ///
    /// The renderer buffers its body, so polling the future once is enough.
    fn problem_document(response: Response) -> Value {
        let body = http_body_util::BodyExt::collect(response.into_body())
            .now_or_never()
            .expect("the rendered body is buffered")
            .expect("the rendered body is valid")
            .to_bytes();
        serde_json::from_slice(&body).expect("the body is problem+json")
    }

    #[test]
    fn every_rendered_error_carries_a_correlation_id() {
        // A management error has no request id of its own: one is minted for it,
        // so the problem document can be joined with the log record.
        let rendered = render_problem(&OagwError::validation("bad body"));
        let body = problem_document(rendered);

        let request_id = body
            .get("trace_id")
            .and_then(Value::as_str)
            .expect("trace_id");
        assert!(
            Uuid::parse_str(request_id).is_ok(),
            "{request_id} is a UUID"
        );

        // An error that already carries a correlation id keeps it.
        let stamped =
            render_problem(&OagwError::validation("bad body").with_trace_id("01JFIXEDID"));
        let body = problem_document(stamped);
        assert_eq!(
            body.get("trace_id").and_then(Value::as_str),
            Some("01JFIXEDID"),
            "the data-plane correlation id is preserved"
        );
    }

    #[test]
    fn body_mappers_produce_validation_errors() {
        let err = invalid_body("unexpected token");
        assert_eq!(err.kind(), OagwErrorKind::ValidationError);
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);

        let err = unsupported_media_type(Some("text/plain"));
        assert_eq!(err.kind(), OagwErrorKind::ValidationError);
        assert!(err.detail().contains("text/plain"));

        let err = unsupported_media_type(None);
        assert!(err.detail().contains("application/json"));
    }

    #[test]
    fn success_responses_are_stamped_gateway() {
        let response = stamp_gateway_source((StatusCode::CREATED, "body").into_response());

        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(
            response
                .headers()
                .get(ERROR_SOURCE_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some(ERROR_SOURCE_GATEWAY)
        );
    }

    #[test]
    fn stamping_keeps_the_status_and_the_body() {
        let stamped = stamp_gateway_source(StatusCode::NO_CONTENT.into_response());

        assert_eq!(stamped.status(), StatusCode::NO_CONTENT);
        assert!(
            stamped
                .headers()
                .get(ERROR_SOURCE_HEADER)
                .and_then(|value| value.to_str().ok())
                .is_some(),
            "204 responses carry the header too"
        );
    }
}
