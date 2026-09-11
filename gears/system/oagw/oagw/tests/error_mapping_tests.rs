//! Error catalogue and RFC 9457 mapping tests.
//!
//! Covers `cpt-cf-oagw-dod-error-catalogue` and `cpt-cf-oagw-algo-error-mapping`:
//! one row per `ErrorKind` variant carrying its HTTP status and full GTS
//! identifier, the §1.5-added 409 variants, the gateway `application/problem+json`
//! envelope, the upstream passthrough that is never rewritten, and the
//! `Retry-After` rule that fires only for the six retriable rows.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::missing_panics_doc)]

use axum::http::{StatusCode, header};
use axum::response::Response;
use oagw::{
    DomainError, ERR_ALIAS_CONFLICT, ERR_AUTH_FAILED, ERR_CIRCUIT_BREAKER_OPEN,
    ERR_DOWNSTREAM_ERROR, ERR_INVALID_TARGET_HOST, ERR_LINK_UNAVAILABLE, ERR_MATCH_CONFLICT,
    ERR_MISSING_TARGET_HOST, ERR_PAYLOAD_TOO_LARGE, ERR_PLUGIN_IN_USE, ERR_PLUGIN_NOT_FOUND,
    ERR_PROTOCOL_ERROR, ERR_RATE_LIMIT_EXCEEDED, ERR_ROUTE_NOT_FOUND, ERR_SECRET_NOT_FOUND,
    ERR_STREAM_ABORTED, ERR_TIMEOUT_CONNECTION, ERR_TIMEOUT_IDLE, ERR_TIMEOUT_REQUEST,
    ERR_UNKNOWN_TARGET_HOST, ERR_VALIDATION, ErrorContext, ErrorKind, ErrorSource,
};

const REQUEST_URI: &str = "/v1/chat/completions";

/// `(variant, expected status, expected GTS instance id)` — the DESIGN §3.3
/// catalogue table, restated.
const CATALOGUE: [(ErrorKind, u16, &str); 22] = [
    (ErrorKind::RouteError, 400, ERR_VALIDATION),
    (ErrorKind::ValidationError, 400, ERR_VALIDATION),
    (ErrorKind::MissingTargetHost, 400, ERR_MISSING_TARGET_HOST),
    (ErrorKind::InvalidTargetHost, 400, ERR_INVALID_TARGET_HOST),
    (ErrorKind::UnknownTargetHost, 400, ERR_UNKNOWN_TARGET_HOST),
    (ErrorKind::AuthenticationFailed, 401, ERR_AUTH_FAILED),
    (ErrorKind::RouteNotFound, 404, ERR_ROUTE_NOT_FOUND),
    (ErrorKind::PluginInUse, 409, ERR_PLUGIN_IN_USE),
    (ErrorKind::AliasConflict, 409, ERR_ALIAS_CONFLICT),
    (ErrorKind::MatchConflict, 409, ERR_MATCH_CONFLICT),
    (ErrorKind::PayloadTooLarge, 413, ERR_PAYLOAD_TOO_LARGE),
    (ErrorKind::RateLimitExceeded, 429, ERR_RATE_LIMIT_EXCEEDED),
    (ErrorKind::SecretNotFound, 500, ERR_SECRET_NOT_FOUND),
    (ErrorKind::ProtocolError, 502, ERR_PROTOCOL_ERROR),
    (ErrorKind::DownstreamError, 502, ERR_DOWNSTREAM_ERROR),
    (ErrorKind::StreamAborted, 502, ERR_STREAM_ABORTED),
    (ErrorKind::LinkUnavailable, 503, ERR_LINK_UNAVAILABLE),
    (ErrorKind::CircuitBreakerOpen, 503, ERR_CIRCUIT_BREAKER_OPEN),
    (ErrorKind::PluginNotFound, 503, ERR_PLUGIN_NOT_FOUND),
    (ErrorKind::ConnectionTimeout, 504, ERR_TIMEOUT_CONNECTION),
    (ErrorKind::RequestTimeout, 504, ERR_TIMEOUT_REQUEST),
    (ErrorKind::IdleTimeout, 504, ERR_TIMEOUT_IDLE),
];

/// The six catalogue rows marked `Yes` in the Retriable column.
const RETRIABLE: [ErrorKind; 6] = [
    ErrorKind::RateLimitExceeded,
    ErrorKind::LinkUnavailable,
    ErrorKind::CircuitBreakerOpen,
    ErrorKind::ConnectionTimeout,
    ErrorKind::RequestTimeout,
    ErrorKind::IdleTimeout,
];

fn gateway(kind: ErrorKind) -> DomainError {
    DomainError::gateway(kind, "caller detail")
}

async fn into_response(
    response: Response,
) -> (StatusCode, axum::http::HeaderMap, serde_json::Value) {
    let (status, headers, bytes) = into_parts(response).await;
    (
        status,
        headers,
        serde_json::from_slice(&bytes).expect("body is JSON"),
    )
}

/// Collects a response without requiring a JSON body.
async fn into_parts(response: Response) -> (StatusCode, axum::http::HeaderMap, bytes::Bytes) {
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = http_body_util::BodyExt::collect(response.into_body())
        .await
        .expect("body collects")
        .to_bytes();
    (status, headers, bytes)
}

#[tokio::test]
async fn every_variant_carries_its_catalogue_row() {
    for (kind, status, gts_type) in CATALOGUE {
        assert_eq!(kind.http_status(), status, "{kind:?} status");
        assert_eq!(kind.gts_type(), gts_type, "{kind:?} GTS type");
        assert_eq!(kind.gts_type().len(), gts_type.len());
    }
}

#[tokio::test]
async fn every_variant_has_a_stable_title() {
    for (kind, _, _) in CATALOGUE {
        let title = kind.title();
        assert!(!title.is_empty(), "{kind:?} title must not be empty");
        assert_eq!(kind.to_string(), title, "{kind:?} Display mirrors title");
    }
}

#[tokio::test]
async fn exactly_the_six_yes_rows_are_retriable() {
    for (kind, _, _) in CATALOGUE {
        assert_eq!(RETRIABLE.contains(&kind), kind.is_retriable(), "{kind:?}");
    }
    assert_eq!(RETRIABLE.len(), 6);
}

#[tokio::test]
async fn the_two_section_1_5_variants_answer_409() {
    assert_eq!(ErrorKind::AliasConflict.http_status(), 409);
    assert_eq!(ErrorKind::MatchConflict.http_status(), 409);
    assert!(!ErrorKind::AliasConflict.is_retriable());
    assert!(!ErrorKind::MatchConflict.is_retriable());
}

#[tokio::test]
async fn downstream_error_is_non_retriable_per_section_1_5() {
    assert_eq!(ErrorKind::DownstreamError.http_status(), 502);
    assert!(!ErrorKind::DownstreamError.is_retriable());
}

#[tokio::test]
async fn gateway_response_is_an_rfc_9457_problem_document() {
    let response =
        oagw::api::rest::problem::problem_response(&gateway(ErrorKind::RouteNotFound), REQUEST_URI);
    let (status, headers, body) = into_response(response).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/problem+json"),
        "gateway failures answer with a problem body"
    );
    assert_eq!(
        headers
            .get("X-OAGW-Error-Source")
            .and_then(|v| v.to_str().ok()),
        Some("gateway")
    );
    assert_eq!(body["type"], ERR_ROUTE_NOT_FOUND);
    assert_eq!(body["title"], ErrorKind::RouteNotFound.title());
    assert_eq!(body["status"], 404);
    assert_eq!(body["detail"], "caller detail");
    assert_eq!(body["instance"], REQUEST_URI);
}

#[tokio::test]
async fn gateway_response_is_problem_json_for_every_variant() {
    for (kind, _, _) in CATALOGUE {
        let response = oagw::api::rest::problem::problem_response(&gateway(kind), REQUEST_URI);
        let (status, headers, body) = into_response(response).await;
        assert_eq!(status.as_u16(), kind.http_status(), "{kind:?}");
        assert_eq!(
            headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/problem+json"),
            "{kind:?} is never rewritten into a generic body"
        );
        assert_eq!(body["type"], kind.gts_type(), "{kind:?}");
        assert_eq!(body["status"], kind.http_status(), "{kind:?}");
    }
}

#[tokio::test]
async fn present_context_members_become_extension_fields() {
    let mut error = gateway(ErrorKind::RateLimitExceeded);
    error.context = ErrorContext {
        upstream_id: Some(uuid::Uuid::nil()),
        host: Some(String::from("api.openai.com")),
        path: Some(String::from("/v1/chat")),
        retry_after_seconds: Some(7),
        trace_id: Some(String::from("trace-1")),
    };

    let (_, _, body) = into_response(oagw::api::rest::problem::problem_response(
        &error,
        REQUEST_URI,
    ))
    .await;

    assert_eq!(body["upstream_id"], uuid::Uuid::nil().to_string());
    assert_eq!(body["host"], "api.openai.com");
    assert_eq!(body["path"], "/v1/chat");
    assert_eq!(body["retry_after_seconds"], 7);
    assert_eq!(body["trace_id"], "trace-1");
}

#[tokio::test]
async fn absent_context_members_add_nothing() {
    let (_, _, body) = into_response(oagw::api::rest::problem::problem_response(
        &gateway(ErrorKind::RouteNotFound),
        REQUEST_URI,
    ))
    .await;

    for field in [
        "upstream_id",
        "host",
        "path",
        "retry_after_seconds",
        "trace_id",
    ] {
        assert!(body.get(field).is_none(), "{field} must be absent");
    }
}

#[tokio::test]
async fn retry_after_is_emitted_only_for_the_six_yes_rows() {
    for kind in RETRIABLE {
        let mut error = gateway(kind);
        error.context.retry_after_seconds = Some(9);
        let (_, headers, _) = into_response(oagw::api::rest::problem::problem_response(
            &error,
            REQUEST_URI,
        ))
        .await;
        assert_eq!(
            headers
                .get(header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some("9"),
            "{kind:?} emits Retry-After"
        );
    }
}

#[tokio::test]
async fn retry_after_is_absent_without_a_supplied_delay() {
    for kind in RETRIABLE {
        let error = gateway(kind);
        let (_, headers, _) = into_response(oagw::api::rest::problem::problem_response(
            &error,
            REQUEST_URI,
        ))
        .await;
        assert!(
            headers.get(header::RETRY_AFTER).is_none(),
            "{kind:?} must not invent a delay"
        );
    }
}

#[tokio::test]
async fn retry_after_is_never_emitted_for_non_retriable_rows() {
    for (kind, _, _) in CATALOGUE {
        if kind.is_retriable() {
            continue;
        }
        let mut error = gateway(kind);
        error.context.retry_after_seconds = Some(9);
        let (_, headers, _) = into_response(oagw::api::rest::problem::problem_response(
            &error,
            REQUEST_URI,
        ))
        .await;
        assert!(
            headers.get(header::RETRY_AFTER).is_none(),
            "{kind:?} must never emit Retry-After"
        );
    }
}

#[tokio::test]
async fn downstream_error_never_emits_retry_after() {
    let mut error = gateway(ErrorKind::DownstreamError);
    error.context.retry_after_seconds = Some(9);
    let (_, headers, _) = into_response(oagw::api::rest::problem::problem_response(
        &error,
        REQUEST_URI,
    ))
    .await;
    assert!(headers.get(header::RETRY_AFTER).is_none());
}

#[tokio::test]
async fn upstream_failure_is_passed_through_not_rewritten() {
    let body = axum::body::Body::from(String::from("{\"error\":\"upstream says no\"}"));
    let response =
        oagw::api::rest::problem::passthrough_response(502, body, Some("application/json"));
    let (status, headers, parsed) = into_response(response).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(
        headers
            .get("X-OAGW-Error-Source")
            .and_then(|v| v.to_str().ok()),
        Some("upstream")
    );
    assert_eq!(
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/json"),
        "the upstream content type is preserved"
    );
    assert_eq!(parsed["error"], "upstream says no");
    assert!(
        parsed.get("type").is_none() && parsed.get("title").is_none(),
        "an upstream failure must never become a problem document"
    );
}

#[tokio::test]
async fn upstream_passthrough_without_a_content_type_adds_none() {
    let response = oagw::api::rest::problem::passthrough_response(
        503,
        axum::body::Body::from(String::from("upstream down")),
        None,
    );
    let (status, headers, _) = into_parts(response).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        headers.get(header::CONTENT_TYPE).is_none(),
        "no problem content type may be invented"
    );
}

#[tokio::test]
async fn passthrough_never_carries_retry_after() {
    let response =
        oagw::api::rest::problem::passthrough_response(429, axum::body::Body::empty(), None);
    let (_, headers, _) = into_parts(response).await;
    assert!(headers.get(header::RETRY_AFTER).is_none());
}

#[tokio::test]
async fn the_mapper_adds_nothing_to_detail() {
    let detail = "upstream refused";
    let mut error = DomainError::gateway(ErrorKind::SecretNotFound, detail);
    error.context = ErrorContext {
        trace_id: Some(String::from("trace-1")),
        ..ErrorContext::default()
    };

    let (_, _, body) = into_response(oagw::api::rest::problem::problem_response(
        &error,
        REQUEST_URI,
    ))
    .await;

    assert_eq!(body["detail"], detail, "detail is passed through verbatim");
    for forbidden in ["cred://", "password", "secret_value", "proxy_timeout_secs"] {
        let rendered = body.to_string();
        assert!(
            !rendered.contains(forbidden),
            "mapper must not inject {forbidden}"
        );
    }
}

#[tokio::test]
async fn domain_error_sources_map_to_their_header_values() {
    assert_eq!(ErrorSource::Gateway.as_str(), "gateway");
    assert_eq!(ErrorSource::Upstream.as_str(), "upstream");
    assert_eq!(ErrorSource::Gateway.to_string(), "gateway");
}

#[tokio::test]
async fn domain_error_carries_its_row() {
    let error = DomainError::upstream(ErrorKind::StreamAborted, "stream cut");
    assert_eq!(error.http_status(), 502);
    assert_eq!(error.gts_type(), ERR_STREAM_ABORTED);
    assert!(!error.is_retriable());
    assert_eq!(error.source, ErrorSource::Upstream);
    assert_eq!(error.to_string(), "Stream aborted: stream cut");
}

#[tokio::test]
async fn retry_after_seconds_is_gated_on_the_row() {
    let mut retriable = DomainError::gateway(ErrorKind::RequestTimeout, "slow");
    retriable.context.retry_after_seconds = Some(3);
    assert_eq!(retriable.retry_after_seconds(), Some(3));

    let mut plain = DomainError::gateway(ErrorKind::DownstreamError, "boom");
    plain.context.retry_after_seconds = Some(3);
    assert_eq!(plain.retry_after_seconds(), None);
}
