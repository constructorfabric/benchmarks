#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::inconsistent_struct_constructor)]
//! Slice 4: provider error classification
//! (`cpt-cf-mini-chat-algo-streaming-turns-classify-provider-error`).

use serde_json::{Value, json};
use toolkit_canonical_errors::resource_error;

use super::*;
use crate::domain::service::stream::types::StreamErrorCode;

#[resource_error(gts_id!("cf.core.oagw.proxy.v1~"))]
struct ProxyScope;

fn response(status: u16, body: &[u8]) -> HttpResponseInfo<'_> {
    HttpResponseInfo {
        status,
        retry_after: None,
        content_type: Some("application/json"),
        source: None,
        body,
    }
}

fn json_body(value: &Value) -> Vec<u8> {
    value.to_string().into_bytes()
}

fn classify(info: &HttpResponseInfo<'_>) -> ProviderFailure {
    classify_response(info).expect("a failure")
}

// ---- proxy_request errors -----------------------------------------------------------------------

#[test]
fn a_deadline_exceeded_call_is_a_provider_timeout() {
    let error =
        ProxyScope::deadline_exceeded("upstream call took too long: secret detail").create();
    let failure = classify_call_error(&error);
    assert_eq!(failure.code, StreamErrorCode::ProviderTimeout);
    assert_eq!(
        failure.message,
        StreamErrorCode::ProviderTimeout.generic_message()
    );
    assert!(
        !failure.message.contains("secret"),
        "the gateway text is never relayed"
    );
    assert_eq!(failure.detail, None);
    assert_eq!(failure.usage, None);
}

#[test]
fn any_other_call_error_is_a_provider_error_with_the_generic_text() {
    for error in [
        toolkit_canonical_errors::CanonicalError::internal("secret internal detail").create(),
        toolkit_canonical_errors::CanonicalError::service_unavailable()
            .with_retry_after_seconds(5)
            .create(),
        ProxyScope::not_found("no route for alias")
            .with_resource("route")
            .create(),
    ] {
        let failure = classify_call_error(&error);
        assert_eq!(failure.code, StreamErrorCode::ProviderError);
        assert_eq!(
            failure.message,
            StreamErrorCode::ProviderError.generic_message()
        );
        assert!(!failure.message.contains("secret"));
    }
}

// ---- responses ----------------------------------------------------------------------------------

#[test]
fn a_two_hundred_event_stream_is_not_a_failure() {
    for content_type in [
        "text/event-stream",
        "text/event-stream; charset=utf-8",
        "TEXT/EVENT-STREAM",
    ] {
        let info = HttpResponseInfo {
            content_type: Some(content_type),
            ..response(200, b"")
        };
        assert_eq!(classify_response(&info), None, "{content_type}");
    }
}

#[test]
fn a_two_hundred_that_is_not_an_event_stream_is_a_provider_error() {
    for content_type in [Some("application/json"), Some("text/html"), None] {
        let info = HttpResponseInfo {
            content_type,
            ..response(200, b"{}")
        };
        let failure = classify(&info);
        assert_eq!(
            failure.code,
            StreamErrorCode::ProviderError,
            "{content_type:?}"
        );
        assert_eq!(
            failure.message,
            StreamErrorCode::ProviderError.generic_message()
        );
    }
}

#[test]
fn another_two_xx_status_with_an_event_stream_passes() {
    let info = HttpResponseInfo {
        content_type: Some("text/event-stream"),
        ..response(203, b"")
    };
    assert_eq!(classify_response(&info), None);
}

#[test]
fn the_gateways_own_504_is_a_provider_timeout() {
    let body = json_body(&json!({
        "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.deadline_exceeded.v1~",
        "status": 504, "detail": "deadline exceeded at the gateway"
    }));
    let info = HttpResponseInfo {
        source: Some(Source::Gateway),
        ..response(504, &body)
    };
    let failure = classify(&info);
    assert_eq!(failure.code, StreamErrorCode::ProviderTimeout);
    assert_eq!(
        failure.message,
        StreamErrorCode::ProviderTimeout.generic_message()
    );
}

#[test]
fn a_provider_504_with_its_own_error_body_is_a_provider_error() {
    let body = json_body(
        &json!({ "error": { "message": "The upstream timed out", "type": "server_error" } }),
    );
    for source in [Some(Source::Upstream), None] {
        let info = HttpResponseInfo {
            source,
            ..response(504, &body)
        };
        let failure = classify(&info);
        assert_eq!(failure.code, StreamErrorCode::ProviderError, "{source:?}");
        assert_eq!(failure.message, "The upstream timed out");
    }
}

#[test]
fn a_429_is_rate_limited_and_names_a_numeric_retry_after() {
    let info = HttpResponseInfo {
        retry_after: Some("7"),
        ..response(429, b"")
    };
    let failure = classify(&info);
    assert_eq!(failure.code, StreamErrorCode::RateLimited);
    assert!(
        failure
            .message
            .starts_with(StreamErrorCode::RateLimited.generic_message())
    );
    assert!(failure.message.contains("7 seconds"), "{}", failure.message);
}

#[test]
fn a_429_without_a_numeric_retry_after_has_only_the_generic_text() {
    for retry_after in [
        None,
        Some("Wed, 21 Oct 2026 07:28:00 GMT"),
        Some("soon"),
        Some(""),
    ] {
        let info = HttpResponseInfo {
            retry_after,
            ..response(429, b"{\"error\":{\"message\":\"quota\"}}")
        };
        let failure = classify(&info);
        assert_eq!(failure.code, StreamErrorCode::RateLimited);
        assert_eq!(
            failure.message,
            StreamErrorCode::RateLimited.generic_message(),
            "{retry_after:?}"
        );
    }
}

#[test]
fn a_429_wins_over_the_error_source() {
    let info = HttpResponseInfo {
        source: Some(Source::Gateway),
        retry_after: Some("2"),
        ..response(429, b"")
    };
    assert_eq!(classify(&info).code, StreamErrorCode::RateLimited);
}

#[test]
fn a_non_success_status_relays_the_sanitized_provider_message() {
    let body = json_body(&json!({
        "error": {
            "code": "invalid_request_error",
            "message": "Invalid file file-abc123def456ghi in resp_9xY2 see https://x.example/a with key sk-ABCDEFGHIJ12"
        }
    }));
    for status in [400, 401, 403, 404, 500, 502, 503] {
        let failure = classify(&response(status, &body));
        assert_eq!(failure.code, StreamErrorCode::ProviderError, "{status}");
        assert_eq!(
            failure.message,
            "Invalid file [provider_id] in [provider_id] see [url] with key [credential]"
        );
        assert_eq!(
            failure.detail.as_deref(),
            Some(
                "invalid_request_error: Invalid file [provider_id] in [provider_id] see [url] with key [credential]"
            )
        );
    }
}

#[test]
fn a_flat_message_body_is_used_too() {
    let body = json_body(&json!({ "message": "Model is overloaded" }));
    let failure = classify(&response(503, &body));
    assert_eq!(failure.message, "Model is overloaded");
    assert_eq!(failure.detail.as_deref(), Some("Model is overloaded"));
}

#[test]
fn an_error_type_serves_as_the_code_of_the_detail_when_there_is_no_code() {
    let body = json_body(&json!({ "error": { "type": "server_error", "message": "boom" } }));
    let failure = classify(&response(500, &body));
    assert_eq!(failure.detail.as_deref(), Some("server_error: boom"));
}

#[test]
fn a_body_without_a_usable_message_is_the_generic_text_and_has_no_detail() {
    for body in [
        b"".to_vec(),
        b"<html>Bad gateway</html>".to_vec(),
        b"{not json".to_vec(),
        json_body(&json!({ "error": {} })),
        json_body(&json!({ "error": { "message": "" } })),
        json_body(&json!({ "error": { "message": 42 } })),
        json_body(&json!({ "other": "x" })),
        json_body(&json!(["a"])),
    ] {
        let failure = classify(&response(500, &body));
        assert_eq!(failure.code, StreamErrorCode::ProviderError);
        assert_eq!(
            failure.message,
            StreamErrorCode::ProviderError.generic_message(),
            "{}",
            String::from_utf8_lossy(&body)
        );
        assert_eq!(failure.detail, None);
    }
}

#[test]
fn a_message_that_sanitizes_to_nothing_is_the_generic_text() {
    let body = json_body(&json!({ "error": { "message": "" } }));
    assert_eq!(
        classify(&response(500, &body)).message,
        StreamErrorCode::ProviderError.generic_message()
    );
}

#[test]
fn the_failure_never_carries_usage() {
    let body = json_body(&json!({ "error": { "message": "x" }, "usage": { "input_tokens": 9 } }));
    assert_eq!(classify(&response(500, &body)).usage, None);
}

#[test]
fn the_detail_is_cut_after_sanitizing() {
    let long = format!("{} 3f2b8c1e-9a4d-4e7f-b6a1-0c5d2e8f9a7b", "x".repeat(1100));
    let body = json_body(&json!({ "error": { "code": "c", "message": long } }));
    let detail = classify(&response(500, &body)).detail.unwrap();
    assert!(detail.chars().count() <= 1024);
    assert!(!detail.contains("3f2b8c1e"));
}

// ---- body read errors ---------------------------------------------------------------------------

#[test]
fn a_timed_out_body_read_is_a_provider_timeout() {
    let error = std::io::Error::new(std::io::ErrorKind::TimedOut, "idle timeout of the upstream");
    let failure = classify_body_error(&error);
    assert_eq!(failure.code, StreamErrorCode::ProviderTimeout);
    assert_eq!(
        failure.message,
        StreamErrorCode::ProviderTimeout.generic_message()
    );
}

#[test]
fn a_deadline_exceeded_canonical_error_in_the_body_is_a_provider_timeout() {
    let error = ProxyScope::deadline_exceeded("stream deadline").create();
    assert_eq!(
        classify_body_error(&error).code,
        StreamErrorCode::ProviderTimeout
    );
}

#[test]
fn any_other_body_error_is_a_provider_error_without_its_text() {
    let error = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset by 10.1.2.3");
    let failure = classify_body_error(&error);
    assert_eq!(failure.code, StreamErrorCode::ProviderError);
    assert_eq!(
        failure.message,
        StreamErrorCode::ProviderError.generic_message()
    );
    assert!(!failure.message.contains("10.1.2.3"));
}
