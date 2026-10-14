//! Provider error mapping and sanitization: every identifier family of the sanitizer with its
//! length floor and token boundary, URLs, `sk-` keys and `Bearer` tokens, the generic fallback
//! for 401, 403 and residual id-like tokens, the fixtures `OpenAI` 500 `request ID req_abc123`,
//! `OpenAI` 429 `organization org-XYZ`, `OpenAI` 401 `sk-proj-****abcd` and Anthropic
//! `req_011CSHoEeqs5C35K2UUqR7Fy`, and the code mapping (429 with and without `Retry-After`,
//! gateway 504, provider 504 with a body).
//!
//! Slice S2-provider-path of `cpt-cf-mini-chat-feature-message-streaming`: algorithm
//! `error-sanitization`; definition of done
//! `cpt-cf-mini-chat-dod-message-streaming-error-sanitization`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mini_chat_sdk::UsageTokens;
use serde_json::json;

use super::provider_errors::{
    MappedFailure, ProviderFailure, generic_message, has_unrecognized_id, map_provider_failure,
    sanitize_provider_text,
};
use super::provider_events::ProviderFailureEvent;
use crate::domain::error::StreamErrorCode;

fn http(status: u16, retry_after: Option<&str>, body: &serde_json::Value) -> ProviderFailure {
    ProviderFailure::Http {
        status,
        retry_after: retry_after.map(ToOwned::to_owned),
        body: body.to_string(),
        from_gateway: false,
    }
}

// ── the sanitizer ────────────────────────────────────────────────────────────

#[test]
fn each_recognized_id_family_is_replaced() {
    let cases = [
        ("id resp_0aB1c2D3e4F5g6H7 here", "id [provider_id] here"),
        ("chatcmpl-AbC123def", "[provider_id]"),
        ("cmpl-abc123", "[provider_id]"),
        ("msg_01XyZ", "[provider_id]"),
        (
            "request ID req_abc123 failed",
            "request ID [provider_id] failed",
        ),
        (
            "organization org-XYZ has hit its limit",
            "organization [provider_id] has hit its limit",
        ),
        ("proj_AbCd1234", "[provider_id]"),
        ("toolu_01AbCdEf", "[provider_id]"),
        ("call_AbCdEfGhIjKlMn", "[provider_id]"),
        ("run_AbCdEfGhIjKlMn", "[provider_id]"),
        ("thread_AbCdEfGhIjKlMn", "[provider_id]"),
        ("file-AbCdEfGhIjKlMn", "[provider_id]"),
        ("file_AbCdEfGhIjKlMn", "[provider_id]"),
        ("assistant-AbCdEfGhIjKlMn", "[provider_id]"),
        ("vs_AbCdEfGhIjKlMn", "[provider_id]"),
    ];
    for (input, expected) in cases {
        assert_eq!(sanitize_provider_text(input), expected, "{input}");
    }
}

#[test]
fn the_twelve_character_floor_keeps_ordinary_words_intact() {
    for untouched in [
        "file-based storage",
        "uses file_search here",
        "the thread_summary job",
        "assistant-like behavior",
        "open vs_code now",
        "call_short",
        "file-AbCdEfGhIjK",
    ] {
        assert_eq!(sanitize_provider_text(untouched), untouched, "{untouched}");
    }
    // exactly twelve letters and digits are replaced
    assert_eq!(sanitize_provider_text("file-AbCdEfGhIjKl"), "[provider_id]");
}

#[test]
fn a_prefix_is_matched_only_at_the_start_of_a_token() {
    for untouched in [
        "xreq_abc123",
        "my-req_abc123",
        "1resp_abc",
        "a_msg_abc",
        "pre-org-XYZ",
    ] {
        assert_eq!(sanitize_provider_text(untouched), untouched, "{untouched}");
    }
    for (input, expected) in [
        ("(req_abc123)", "([provider_id])"),
        ("id=req_abc123,", "id=[provider_id],"),
        ("\"resp_abc123\"", "\"[provider_id]\""),
    ] {
        assert_eq!(sanitize_provider_text(input), expected, "{input}");
    }
}

#[test]
fn request_id_style_tokens_are_replaced() {
    assert_eq!(
        sanitize_provider_text("trace 550e8400-e29b-41d4-a716-446655440000 end"),
        "trace [provider_id] end"
    );
    assert_eq!(
        sanitize_provider_text("hash deadbeefdeadbeef0123 end"),
        "hash [provider_id] end"
    );
    // fifteen hexadecimal characters are an ordinary word
    assert_eq!(sanitize_provider_text("deadbeefdeadbee"), "deadbeefdeadbee");
}

#[test]
fn urls_become_url() {
    assert_eq!(
        sanitize_provider_text("see https://platform.openai.com/docs/x?y=1 now"),
        "see [url] now"
    );
    assert_eq!(sanitize_provider_text("http://localhost:8080/v1"), "[url]");
    assert_eq!(
        sanitize_provider_text("resp at https://api.example.com/resp_abc123"),
        "resp at [url]"
    );
}

#[test]
fn keys_and_bearer_tokens_become_credential() {
    assert_eq!(
        sanitize_provider_text("Incorrect API key provided: sk-proj-****abcd."),
        "Incorrect API key provided: [credential]."
    );
    assert_eq!(sanitize_provider_text("sk-AbCdEfGhIj1234"), "[credential]");
    assert_eq!(
        sanitize_provider_text("header Bearer abc.def-123 rejected"),
        "header [credential] rejected"
    );
    // a short `sk-` word is not a key
    assert_eq!(sanitize_provider_text("sk-short"), "sk-short");
}

#[test]
fn text_without_identifiers_is_left_unchanged() {
    let text = "The model is overloaded. Please try again in a few minutes.";
    assert_eq!(sanitize_provider_text(text), text);
    assert_eq!(sanitize_provider_text(""), "");
}

#[test]
fn a_residual_id_like_token_is_detected() {
    for suspicious in [
        "abc123def456ghi789jkl",
        "abc_def_123_ghi_456_jk",
        "see Zk9Qm2Xv7Lp4Nb8Rt1Yc3Wd please",
    ] {
        assert!(has_unrecognized_id(suspicious), "{suspicious}");
    }
    for fine in [
        "abcdefghijklmnopqrstuvwxyz",
        "12345678901234567890123",
        "short1a2b",
        "The model is overloaded",
        "[provider_id] and [credential] and [url]",
    ] {
        assert!(!has_unrecognized_id(fine), "{fine}");
    }
}

// ── the mapping ──────────────────────────────────────────────────────────────

fn openai_500() -> serde_json::Value {
    json!({ "error": {
        "message": "The server had an error while processing your request. Sorry about that! \
                    (Please include the request ID req_abc123 in your message.)",
        "type": "server_error", "code": null } })
}

fn assert_clean(mapped: &MappedFailure, hidden: &[&str]) {
    for needle in hidden {
        assert!(
            !mapped.message.contains(needle),
            "`{needle}` in {:?}",
            mapped.message
        );
    }
}

#[test]
fn a_provider_429_is_rate_limited_and_carries_the_retry_after_seconds() {
    let body = json!({ "error": {
        "message": "Rate limit reached for gpt-x in organization org-XYZ on requests per min.",
        "type": "requests", "code": "rate_limit_exceeded" } });
    let mapped = map_provider_failure(&http(429, Some("30"), &body));
    assert_eq!(mapped.code, StreamErrorCode::RateLimited);
    assert!(mapped.message.contains("30"), "{}", mapped.message);
    assert_clean(&mapped, &["org-XYZ"]);
    assert!(
        mapped.message.contains("[provider_id]"),
        "{}",
        mapped.message
    );
    // the provider text stays available for the logs
    assert!(mapped.internal.contains("org-XYZ"), "{}", mapped.internal);
}

#[test]
fn a_429_without_a_numeric_retry_after_carries_no_delay() {
    let body = json!({ "error": { "message": "Too many requests" } });
    let none = map_provider_failure(&http(429, None, &body));
    assert_eq!(none.code, StreamErrorCode::RateLimited);
    let date = map_provider_failure(&http(429, Some("Wed, 21 Oct 2026 07:28:00 GMT"), &body));
    assert_eq!(date.code, StreamErrorCode::RateLimited);
    assert!(!date.message.contains("Oct"), "{}", date.message);
    assert!(!date.message.contains("GMT"), "{}", date.message);
}

#[test]
fn a_gateway_timeout_and_the_gateway_504_problem_are_provider_timeout() {
    assert_eq!(
        map_provider_failure(&ProviderFailure::GatewayTimeout).code,
        StreamErrorCode::ProviderTimeout
    );
    let problem = json!({ "type": "gts.cf.core.errors.err.v1~cf.core.err.deadline_exceeded.v1~",
                           "title": "Deadline exceeded", "status": 504 });
    let gateway = ProviderFailure::Http {
        status: 504,
        retry_after: None,
        body: problem.to_string(),
        from_gateway: true,
    };
    assert_eq!(
        map_provider_failure(&gateway).code,
        StreamErrorCode::ProviderTimeout
    );
    assert_eq!(
        map_provider_failure(&ProviderFailure::IdleTimeout).code,
        StreamErrorCode::ProviderTimeout
    );
}

#[test]
fn a_provider_504_with_its_own_body_is_a_provider_error() {
    let mapped = map_provider_failure(&http(
        504,
        None,
        &json!({ "error": { "message": "upstream request timeout" } }),
    ));
    assert_eq!(mapped.code, StreamErrorCode::ProviderError);
}

#[test]
fn an_openai_500_keeps_its_text_with_the_request_id_replaced() {
    let mapped = map_provider_failure(&http(500, None, &openai_500()));
    assert_eq!(mapped.code, StreamErrorCode::ProviderError);
    assert!(
        mapped.message.contains("The server had an error"),
        "{}",
        mapped.message
    );
    assert!(
        mapped.message.contains("[provider_id]"),
        "{}",
        mapped.message
    );
    assert_clean(&mapped, &["req_abc123"]);
}

#[test]
fn an_anthropic_request_id_does_not_survive_in_the_client_text() {
    let in_body = json!({ "type": "error",
        "error": { "type": "api_error", "message": "Internal server error" },
        "request_id": "req_011CSHoEeqs5C35K2UUqR7Fy" });
    let mapped = map_provider_failure(&http(500, None, &in_body));
    assert_eq!(mapped.code, StreamErrorCode::ProviderError);
    assert_clean(&mapped, &["req_011CSHoEeqs5C35K2UUqR7Fy"]);

    let in_message = json!({ "type": "error", "error": { "type": "api_error",
        "message": "Internal server error req_011CSHoEeqs5C35K2UUqR7Fy" } });
    let mapped = map_provider_failure(&http(500, None, &in_message));
    assert!(
        mapped.message.contains("[provider_id]"),
        "{}",
        mapped.message
    );
    assert_clean(&mapped, &["req_011CSHoEeqs5C35K2UUqR7Fy"]);
}

#[test]
fn a_401_and_a_403_use_the_generic_message_and_never_provider_text() {
    let body = json!({ "error": {
        "message": "Incorrect API key provided: sk-proj-****abcd.",
        "type": "invalid_request_error", "code": "invalid_api_key" } });
    for status in [401, 403] {
        let mapped = map_provider_failure(&http(status, None, &body));
        assert_eq!(mapped.code, StreamErrorCode::ProviderError, "{status}");
        assert_eq!(
            mapped.message,
            generic_message(StreamErrorCode::ProviderError),
            "{status}"
        );
        assert_clean(&mapped, &["sk-", "abcd", "Incorrect"]);
    }
}

#[test]
fn a_body_without_a_message_gives_the_generic_message() {
    for body in ["", "<html>502 Bad Gateway</html>", "{}", "{\"error\":{}}"] {
        let mapped = map_provider_failure(&ProviderFailure::Http {
            status: 502,
            retry_after: None,
            body: body.to_owned(),
            from_gateway: false,
        });
        assert_eq!(mapped.code, StreamErrorCode::ProviderError, "{body}");
        assert_eq!(
            mapped.message,
            generic_message(StreamErrorCode::ProviderError),
            "{body}"
        );
    }
}

#[test]
fn a_residual_id_like_token_falls_back_to_the_generic_message() {
    let mapped = map_provider_failure(&http(
        500,
        None,
        &json!({ "error": { "message": "Failed for tenant abc123def456ghi789jkl0 today" } }),
    ));
    assert_eq!(
        mapped.message,
        generic_message(StreamErrorCode::ProviderError)
    );
}

#[test]
fn a_failed_event_maps_to_provider_error_keeps_its_usage_and_sanitizes_the_message() {
    let usage = UsageTokens {
        input_tokens: 80,
        output_tokens: 12,
        ..UsageTokens::default()
    };
    let mapped = map_provider_failure(&ProviderFailure::Event(ProviderFailureEvent {
        code: Some("server_error".to_owned()),
        message: Some("Boom resp_0aB1c2D3e4F5g6H7 see https://x.example/y".to_owned()),
        usage: Some(usage),
    }));
    assert_eq!(mapped.code, StreamErrorCode::ProviderError);
    assert_eq!(mapped.message, "Boom [provider_id] see [url]");
    assert_eq!(mapped.usage, Some(usage));
    assert!(
        mapped.internal.contains("resp_0aB1c2D3e4F5g6H7"),
        "{}",
        mapped.internal
    );
    assert!(
        mapped.internal.contains("server_error"),
        "{}",
        mapped.internal
    );
}

#[test]
fn a_failed_event_without_a_message_gives_the_generic_message() {
    let mapped = map_provider_failure(&ProviderFailure::Event(ProviderFailureEvent {
        code: Some("server_error".to_owned()),
        message: None,
        usage: None,
    }));
    assert_eq!(mapped.code, StreamErrorCode::ProviderError);
    assert_eq!(
        mapped.message,
        generic_message(StreamErrorCode::ProviderError)
    );
    assert_eq!(mapped.usage, None);
}

#[test]
fn transport_failures_and_a_broken_stream_are_provider_errors_without_provider_detail() {
    let transport = map_provider_failure(&ProviderFailure::Transport(
        "connect error to https://10.0.0.5:443/v1/responses".to_owned(),
    ));
    assert_eq!(transport.code, StreamErrorCode::ProviderError);
    assert_clean(&transport, &["10.0.0.5", "https"]);
    let broken = map_provider_failure(&ProviderFailure::StreamBroken);
    assert_eq!(broken.code, StreamErrorCode::ProviderError);
}

#[test]
fn generic_messages_exist_for_every_streaming_code_and_hold_no_identifier() {
    for code in StreamErrorCode::ALL {
        let text = generic_message(code);
        assert!(!text.is_empty(), "{code:?}");
        assert!(text.is_ascii(), "{code:?}");
        assert!(!has_unrecognized_id(text), "{code:?}");
        assert_eq!(sanitize_provider_text(text), text, "{code:?}");
    }
}
