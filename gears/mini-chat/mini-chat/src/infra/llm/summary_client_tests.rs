#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 5: the non-streaming summary call
//! (`cpt-cf-mini-chat-algo-background-workers-call-summary-model`): the request
//! of each provider kind, the response reader, the context-length
//! classification and the client over the fake OAGW. No credential header is
//! ever set.

use std::sync::Arc;

use serde_json::{Value, json};
use uuid::Uuid;

use super::{
    OagwSummaryClient, build_summary_request, is_context_length_error, read_summary_answer,
};
use crate::config::ProviderKind;
use crate::domain::service::stream::ports::ProviderTarget;
use crate::domain::service::workers::SYSTEM_SUBJECT;
use crate::domain::service::workers::ports::{
    SummaryCallError, SummaryModelClient, SummaryRequest,
};
use crate::test_doubles::{FakeOagw, Fault, ProxyReply, S2S_SUBJECT, s2s_ctx};
use crate::worker_test_env::small_model;

const TENANT: Uuid = Uuid::from_u128(0x7E);
const CHAT: Uuid = Uuid::from_u128(0xC1);

fn request(kind: ProviderKind, alias: &str, api_path: &str) -> SummaryRequest {
    SummaryRequest {
        target: ProviderTarget {
            kind,
            alias: alias.to_owned(),
            api_path: api_path.to_owned(),
            api_version: None,
        },
        entry: small_model("sum", ""),
        system: "SYSTEM PROMPT".to_owned(),
        user: "USER PROMPT".to_owned(),
        tenant_id: TENANT,
        chat_id: CHAT,
    }
}

fn openai() -> SummaryRequest {
    request(
        ProviderKind::OpenaiResponses,
        "api.openai.test",
        "/v1/responses",
    )
}

fn user_value() -> String {
    format!("{}{}", TENANT.simple(), SYSTEM_SUBJECT.simple())
}

fn no_credentials(headers: &[(String, String)]) {
    for (name, _) in headers {
        let lower = name.to_ascii_lowercase();
        assert!(
            ![
                "authorization",
                "x-api-key",
                "api-key",
                "proxy-authorization"
            ]
            .contains(&lower.as_str()),
            "no credential header: {name}"
        );
    }
}

// ---- the request -------------------------------------------------------------------------------------------------

#[test]
fn the_system_subject_is_the_default_subject_of_the_platform() {
    assert_eq!(
        SYSTEM_SUBJECT.to_string(),
        "11111111-6a88-4768-9dfc-6bcd5187d9ed"
    );
}

#[test]
fn an_openai_responses_request_is_non_streaming_with_instructions_one_input_and_metadata() {
    let built = build_summary_request(&openai());

    assert_eq!(built.kind, ProviderKind::OpenaiResponses);
    assert_eq!(built.uri, "/api.openai.test/v1/responses");
    let body = &built.body;
    assert_eq!(body["model"], "provider-sum");
    assert_eq!(body["instructions"], "SYSTEM PROMPT");
    assert_eq!(
        body["input"],
        json!([{ "role": "user", "content": [ { "type": "input_text", "text": "USER PROMPT" } ] }])
    );
    assert_eq!(
        body["max_output_tokens"], 100,
        "the catalog max_output_tokens"
    );
    assert_eq!(body["stream"], false);
    assert_eq!(body["store"], false);
    assert_eq!(body["user"], user_value());
    assert_eq!(
        body["metadata"],
        json!({
            "tenant_id": TENANT.to_string(),
            "user_id": SYSTEM_SUBJECT.to_string(),
            "chat_id": CHAT.to_string(),
            "request_type": "summary",
            "feature": "none",
        })
    );
    assert!(body.get("tools").is_none(), "no tools");
    assert!(body.get("tool_choice").is_none());
    no_credentials(&built.headers);
}

#[test]
fn the_output_limit_is_the_catalog_value_not_the_streaming_cap() {
    let mut big = openai();
    big.entry.max_output_tokens = 65_000;

    let built = build_summary_request(&big);

    assert_eq!(built.body["max_output_tokens"], 65_000);
}

#[test]
fn the_model_placeholder_and_the_query_of_the_api_path_are_kept() {
    let mut azure = openai();
    azure.target.api_path = "/openai/deployments/{model}/responses?api-version=2025-01".to_owned();

    let built = build_summary_request(&azure);

    assert_eq!(
        built.uri,
        "/api.openai.test/openai/deployments/provider-sum/responses?api-version=2025-01"
    );
}

#[test]
fn a_vllm_request_is_the_responses_format_without_metadata() {
    let built = build_summary_request(&request(
        ProviderKind::VllmResponses,
        "vllm.test",
        "/v1/responses",
    ));

    assert_eq!(built.kind, ProviderKind::VllmResponses);
    assert_eq!(built.body["instructions"], "SYSTEM PROMPT");
    assert!(built.body.get("metadata").is_none());
    assert_eq!(built.body["stream"], false);
    assert!(built.body.get("tools").is_none());
}

#[test]
fn a_chat_completions_request_has_a_system_and_a_user_message() {
    let built = build_summary_request(&request(
        ProviderKind::OpenaiChatCompletions,
        "chat.test",
        "/v1/chat/completions",
    ));

    assert_eq!(built.uri, "/chat.test/v1/chat/completions");
    let body = &built.body;
    assert_eq!(body["model"], "provider-sum");
    assert_eq!(
        body["messages"],
        json!([
            { "role": "system", "content": "SYSTEM PROMPT" },
            { "role": "user", "content": "USER PROMPT" },
        ])
    );
    assert_eq!(body["max_completion_tokens"], 100);
    assert_eq!(body["stream"], false);
    assert!(body.get("stream_options").is_none());
    assert_eq!(body["user"], user_value());
    assert!(body.get("tools").is_none());
    no_credentials(&built.headers);
}

#[test]
fn an_anthropic_request_has_a_system_field_one_user_message_and_the_version_header() {
    let built = build_summary_request(&request(
        ProviderKind::AnthropicMessages,
        "api.anthropic.test",
        "/v1/messages",
    ));

    let body = &built.body;
    assert_eq!(body["system"], "SYSTEM PROMPT");
    assert_eq!(
        body["messages"],
        json!([{ "role": "user", "content": [ { "type": "text", "text": "USER PROMPT" } ] }])
    );
    assert_eq!(body["max_tokens"], 100);
    assert_eq!(body["stream"], false);
    assert_eq!(body["metadata"], json!({ "user_id": user_value() }));
    assert!(body.get("tools").is_none());
    assert!(
        built
            .headers
            .iter()
            .any(|(name, value)| name == "anthropic-version" && value == "2023-06-01")
    );
    no_credentials(&built.headers);
}

#[test]
fn every_request_asks_for_json_not_an_event_stream() {
    for kind in [
        ProviderKind::OpenaiResponses,
        ProviderKind::VllmResponses,
        ProviderKind::OpenaiChatCompletions,
        ProviderKind::AnthropicMessages,
    ] {
        let built = build_summary_request(&request(kind, "h", "/p"));

        let accept = built
            .headers
            .iter()
            .find(|(name, _)| name == "Accept")
            .map(|(_, v)| v.as_str());
        assert_eq!(accept, Some("application/json"), "{kind:?}");
        assert_eq!(built.body["stream"], false, "{kind:?}");
    }
}

// ---- the response -----------------------------------------------------------------------------------------------

#[test]
fn a_responses_answer_yields_the_output_text_and_the_usage() {
    let body = json!({
        "output": [
            { "type": "reasoning", "summary": [] },
            { "type": "message", "content": [
                { "type": "output_text", "text": "<summary>" },
                { "type": "output_text", "text": "Hi</summary>" },
                { "type": "refusal", "refusal": "no" }
            ] }
        ],
        "usage": {
            "input_tokens": 100, "output_tokens": 40,
            "input_tokens_details": { "cached_tokens": 30 },
            "output_tokens_details": { "reasoning_tokens": 9 }
        }
    });

    let answer = read_summary_answer(ProviderKind::OpenaiResponses, &body).unwrap();

    assert_eq!(answer.text, "<summary>Hi</summary>");
    assert_eq!(
        (
            answer.usage.input_tokens,
            answer.usage.output_tokens,
            answer.usage.cache_read_input_tokens,
            answer.usage.cache_write_input_tokens,
            answer.usage.reasoning_tokens
        ),
        (100, 40, 30, 0, 9)
    );
}

#[test]
fn a_missing_usage_field_is_zero() {
    let body = json!({ "output": [ { "type": "message", "content": [ { "type": "output_text", "text": "x" } ] } ] });

    let answer = read_summary_answer(ProviderKind::VllmResponses, &body).unwrap();

    assert_eq!(answer.text, "x");
    assert_eq!(
        (
            answer.usage.input_tokens,
            answer.usage.output_tokens,
            answer.usage.reasoning_tokens
        ),
        (0, 0, 0)
    );
}

#[test]
fn a_chat_completions_answer_yields_the_first_choice_and_its_usage() {
    let body = json!({
        "choices": [ { "message": { "role": "assistant", "content": "the text" } }, { "message": { "content": "other" } } ],
        "usage": {
            "prompt_tokens": 50, "completion_tokens": 20,
            "prompt_tokens_details": { "cached_tokens": 5 },
            "completion_tokens_details": { "reasoning_tokens": 3 }
        }
    });

    let answer = read_summary_answer(ProviderKind::OpenaiChatCompletions, &body).unwrap();

    assert_eq!(answer.text, "the text");
    assert_eq!(
        (
            answer.usage.input_tokens,
            answer.usage.output_tokens,
            answer.usage.cache_read_input_tokens,
            answer.usage.reasoning_tokens
        ),
        (50, 20, 5, 3)
    );
}

#[test]
fn an_anthropic_answer_joins_the_text_blocks_and_reads_the_cache_counters() {
    let body = json!({
        "content": [
            { "type": "thinking", "thinking": "hmm" },
            { "type": "text", "text": "part one " },
            { "type": "text", "text": "part two" }
        ],
        "usage": {
            "input_tokens": 70, "output_tokens": 33,
            "cache_read_input_tokens": 11, "cache_creation_input_tokens": 4
        }
    });

    let answer = read_summary_answer(ProviderKind::AnthropicMessages, &body).unwrap();

    assert_eq!(answer.text, "part one part two");
    assert_eq!(
        (
            answer.usage.input_tokens,
            answer.usage.output_tokens,
            answer.usage.cache_read_input_tokens,
            answer.usage.cache_write_input_tokens
        ),
        (70, 33, 11, 4)
    );
}

#[test]
fn a_body_without_the_output_of_its_format_cannot_be_read() {
    for (kind, body) in [
        (ProviderKind::OpenaiResponses, json!({ "id": "x" })),
        (ProviderKind::OpenaiResponses, json!({ "output": "text" })),
        (
            ProviderKind::OpenaiChatCompletions,
            json!({ "choices": [] }),
        ),
        (ProviderKind::OpenaiChatCompletions, json!({})),
        (ProviderKind::AnthropicMessages, json!({ "type": "error" })),
        (ProviderKind::AnthropicMessages, json!([])),
    ] {
        assert!(
            read_summary_answer(kind, &body).is_none(),
            "{kind:?} {body}"
        );
    }
}

#[test]
fn an_answer_with_no_text_is_readable_and_empty() {
    let body = json!({ "output": [ { "type": "reasoning" } ] });

    let answer = read_summary_answer(ProviderKind::OpenaiResponses, &body).unwrap();

    assert_eq!(answer.text, "");
}

// ---- the context-length classification -----------------------------------------------------------------------------

fn error_body(code: Option<&str>, message: &str) -> Vec<u8> {
    let mut error = json!({ "message": message });
    if let Some(code) = code {
        error["code"] = json!(code);
    }
    serde_json::to_vec(&json!({ "error": error })).unwrap()
}

#[test]
fn the_context_length_code_and_the_known_messages_are_classified() {
    assert!(is_context_length_error(
        400,
        &error_body(Some("context_length_exceeded"), "x")
    ));
    assert!(is_context_length_error(
        400,
        &error_body(None, "This model's maximum context length is 128000 tokens")
    ));
    assert!(is_context_length_error(
        400,
        &error_body(None, "prompt is too long: 210000 tokens > 200000")
    ));
    assert!(is_context_length_error(
        413,
        &error_body(None, "Request exceeds the context window")
    ));
    assert!(is_context_length_error(
        422,
        &error_body(Some("context_length_exceeded"), "")
    ));
}

#[test]
fn other_errors_are_not_context_length_errors() {
    assert!(!is_context_length_error(
        400,
        &error_body(Some("invalid_request_error"), "bad field")
    ));
    assert!(!is_context_length_error(
        500,
        &error_body(Some("context_length_exceeded"), "x")
    ));
    assert!(!is_context_length_error(
        429,
        &error_body(None, "rate limit")
    ));
    assert!(!is_context_length_error(400, b"not json"));
    assert!(!is_context_length_error(400, b""));
}

// ---- the client over the fake OAGW ----------------------------------------------------------------------------------

fn client(oagw: &Arc<FakeOagw>) -> OagwSummaryClient {
    OagwSummaryClient::new(oagw.clone())
}

fn ok_body(text: &str) -> Value {
    json!({
        "output": [ { "type": "message", "content": [ { "type": "output_text", "text": text } ] } ],
        "usage": { "input_tokens": 12, "output_tokens": 7 }
    })
}

#[tokio::test]
async fn a_successful_call_posts_the_request_with_the_s2s_context_and_reads_the_answer() {
    let oagw = FakeOagw::new();
    oagw.on_path_json(
        "POST",
        "/api.openai.test/v1/responses",
        200,
        &ok_body("<summary>S</summary>"),
    );

    let answer = client(&oagw)
        .summarize(&s2s_ctx(), &openai())
        .await
        .unwrap();

    assert_eq!(answer.text, "<summary>S</summary>");
    assert_eq!(answer.usage.output_tokens, 7);
    let calls = oagw.proxied();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].method, "POST");
    assert_eq!(calls[0].subject, S2S_SUBJECT);
    assert_eq!(calls[0].json()["instructions"], "SYSTEM PROMPT");
    no_credentials(&calls[0].headers);
}

#[tokio::test]
async fn a_non_2xx_answer_is_a_failure_with_a_sanitized_cause() {
    let oagw = FakeOagw::new();
    oagw.on_path_json(
        "POST",
        "/api.openai.test/v1/responses",
        500,
        &json!({ "error": { "message": "secret internal detail" } }),
    );

    let error = client(&oagw)
        .summarize(&s2s_ctx(), &openai())
        .await
        .unwrap_err();

    assert_eq!(error, SummaryCallError::Failed("http 500".to_owned()));
}

#[tokio::test]
async fn a_context_length_error_is_reported_as_such() {
    let oagw = FakeOagw::new();
    oagw.on_path_json(
        "POST",
        "/api.openai.test/v1/responses",
        400,
        &json!({ "error": { "code": "context_length_exceeded", "message": "too long" } }),
    );

    let error = client(&oagw)
        .summarize(&s2s_ctx(), &openai())
        .await
        .unwrap_err();

    assert_eq!(error, SummaryCallError::ContextLength);
}

#[tokio::test]
async fn a_gateway_failure_is_a_failure_named_gateway_error() {
    let oagw = FakeOagw::new();
    oagw.on("POST", "api.openai.test", |_| {
        ProxyReply::Error(Fault::Conflict)
    });

    let error = client(&oagw)
        .summarize(&s2s_ctx(), &openai())
        .await
        .unwrap_err();

    assert_eq!(error, SummaryCallError::Failed("gateway error".to_owned()));
}

#[tokio::test]
async fn a_body_that_is_not_json_or_has_no_output_is_unreadable() {
    for reply in [
        ProxyReply::Response {
            status: 200,
            headers: vec![],
            body: crate::test_doubles::ReplyBody::Bytes(b"<html>oops</html>".to_vec()),
            source: None,
        },
        ProxyReply::json(200, &json!({ "unexpected": true })),
    ] {
        let oagw = FakeOagw::new();
        oagw.push_proxy(reply);

        let error = client(&oagw)
            .summarize(&s2s_ctx(), &openai())
            .await
            .unwrap_err();

        assert_eq!(
            error,
            SummaryCallError::Failed("unreadable response".to_owned())
        );
    }
}

#[tokio::test]
async fn a_body_over_one_mebibyte_is_a_failure() {
    let oagw = FakeOagw::new();
    let big = "a".repeat(1024 * 1024 + 10);
    oagw.push_proxy(ProxyReply::json(200, &ok_body(&big)));

    let error = client(&oagw)
        .summarize(&s2s_ctx(), &openai())
        .await
        .unwrap_err();

    assert!(matches!(error, SummaryCallError::Failed(_)), "{error:?}");
}

#[tokio::test]
async fn a_body_just_under_the_bound_is_read() {
    let oagw = FakeOagw::new();
    let text = "a".repeat(900 * 1024);
    oagw.push_proxy(ProxyReply::json(200, &ok_body(&text)));

    let answer = client(&oagw)
        .summarize(&s2s_ctx(), &openai())
        .await
        .unwrap();

    assert_eq!(answer.text.len(), 900 * 1024);
}
