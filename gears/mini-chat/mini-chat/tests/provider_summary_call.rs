//! The non-streaming thread-summary call of the `openai_responses` adapter (DESIGN 3.6 "Thread
//! Summary Update": the summary request is a plain request / response, not a stream). The request
//! says `"stream": false` with `Accept: application/json`, the JSON Responses body is translated
//! into the events the worker consumes (the text, then the completion with its usage), the
//! response `Content-Type` dispatches (an SSE answer is still accepted), and the provider error
//! mapping and sanitization are those of a chat turn. Chat requests keep streaming.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::use_debug,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value
)]

mod chat_support;
mod common;
mod stream_support;

use futures::StreamExt;
use mini_chat::domain::context::HistoryRole;
use mini_chat::domain::provider::{
    ChatInputItem, ChatRequest, ProviderErrorKind, ProviderEvent, ProviderFailure,
};
use mini_chat::infra::llm::openai_responses::{build_body, parse_response_body, stream_chat};
use mini_chat_sdk::ModelApiParams;
use serde_json::{Value, json};
use stream_support::{FakeOagw, Opts, ProviderReply};
use uuid::Uuid;

fn request(is_summary: bool) -> ChatRequest {
    ChatRequest {
        tenant_id: Uuid::from_u128(1),
        user_id: Uuid::from_u128(2),
        chat_id: Uuid::from_u128(3),
        provider_id: "openai".to_owned(),
        provider_model_id: "provider-model".to_owned(),
        instructions: "summarize".to_owned(),
        input: vec![ChatInputItem {
            role: HistoryRole::User,
            content: "the conversation".to_owned(),
            images: Vec::new(),
        }],
        tools: Vec::new(),
        max_output_tokens: 1024,
        max_tool_calls: 2,
        api_params: ModelApiParams {
            temperature: None,
            top_p: None,
            frequency_penalty: None,
            presence_penalty: None,
            stop: Vec::new(),
            extra_body: None,
            reasoning_effort: None,
        },
        is_summary,
    }
}

fn ctx() -> toolkit_security::SecurityContext {
    toolkit_security::SecurityContext::builder()
        .subject_id(Uuid::from_u128(2))
        .subject_type("service")
        .subject_tenant_id(Uuid::from_u128(1))
        .build()
        .unwrap()
}

/// Runs `stream_chat` against the fake and collects every event.
async fn run(
    oagw: &FakeOagw,
    request: &ChatRequest,
) -> Result<Vec<Result<ProviderEvent, ProviderFailure>>, ProviderFailure> {
    let stream = stream_chat(oagw, &ctx(), "alias", "/v1/responses", request).await?;
    Ok(stream.collect().await)
}

fn events(items: Vec<Result<ProviderEvent, ProviderFailure>>) -> Vec<ProviderEvent> {
    items.into_iter().map(|item| item.unwrap()).collect()
}

fn failure_of(
    result: Result<Vec<Result<ProviderEvent, ProviderFailure>>, ProviderFailure>,
) -> ProviderFailure {
    match result {
        Err(failure) => failure,
        Ok(items) => panic!("expected a failed call, got {} events", items.len()),
    }
}

// ── the request ──────────────────────────────────────────────────────────────

#[test]
fn the_summary_body_is_not_streamed_and_a_chat_body_is() {
    assert_eq!(build_body(&request(true))["stream"], json!(false));
    assert_eq!(build_body(&request(false))["stream"], json!(true));
}

#[test]
fn extra_body_cannot_turn_the_summary_stream_on() {
    let mut summary = request(true);
    summary.api_params.extra_body = Some(json!({"stream": true}));
    assert_eq!(build_body(&summary)["stream"], json!(false));
}

#[tokio::test]
async fn the_summary_request_accepts_json_and_a_chat_request_accepts_an_event_stream() {
    let oagw = FakeOagw::new();
    oagw.push(ProviderReply::json_text("a summary", &Opts::default()));
    events(run(&oagw, &request(true)).await.unwrap());
    let recorded = oagw.last_request();
    assert_eq!(recorded.body["stream"], json!(false));
    assert_eq!(recorded.headers.get("accept").unwrap(), "application/json");
    assert_eq!(recorded.body["metadata"]["request_type"], json!("summary"));

    oagw.push(ProviderReply::text(&["hi"]));
    events(run(&oagw, &request(false)).await.unwrap());
    let recorded = oagw.last_request();
    assert_eq!(recorded.body["stream"], json!(true));
    assert_eq!(recorded.headers.get("accept").unwrap(), "text/event-stream");
}

// ── the JSON body ────────────────────────────────────────────────────────────

#[test]
fn a_completed_body_is_the_text_then_the_completion_with_its_usage() {
    let body = json!({
        "id": "resp_json1",
        "status": "completed",
        "output": [
            {"type": "reasoning", "summary": []},
            {"type": "message", "role": "assistant", "content": [
                {"type": "output_text", "text": "Hello ", "annotations": []},
                {"type": "refusal", "refusal": "ignored"},
                {"type": "output_text", "text": "world", "annotations": []}
            ]}
        ],
        "usage": {
            "input_tokens": 100, "output_tokens": 40,
            "input_tokens_details": {"cached_tokens": 30},
            "output_tokens_details": {"reasoning_tokens": 7}
        }
    });
    let parsed = parse_response_body(body.to_string().as_bytes()).unwrap();
    assert_eq!(parsed.len(), 2);
    assert_eq!(
        parsed[0],
        ProviderEvent::TextDelta("Hello world".to_owned())
    );
    let ProviderEvent::Completed(completion) = &parsed[1] else {
        panic!("{:?}", parsed[1]);
    };
    assert_eq!(completion.response_id.as_deref(), Some("resp_json1"));
    assert!(!completion.incomplete);
    assert_eq!(
        (completion.input_tokens, completion.output_tokens),
        (100, 40)
    );
    assert_eq!(completion.breakdown.cache_read, 30);
    assert_eq!(completion.breakdown.reasoning, 7);
}

#[test]
fn an_incomplete_body_is_an_incomplete_completion_and_a_missing_usage_is_zero() {
    let body = json!({"id": "r", "status": "incomplete", "output": [
        {"type": "message", "content": [{"type": "output_text", "text": "cut"}]}
    ]});
    let parsed = parse_response_body(body.to_string().as_bytes()).unwrap();
    assert_eq!(parsed[0], ProviderEvent::TextDelta("cut".to_owned()));
    let ProviderEvent::Completed(completion) = &parsed[1] else {
        panic!("{:?}", parsed[1]);
    };
    assert!(completion.incomplete);
    assert_eq!((completion.input_tokens, completion.output_tokens), (0, 0));
}

#[test]
fn a_body_without_text_is_only_a_completion() {
    let parsed = parse_response_body(br#"{"status":"completed","output":[]}"#).unwrap();
    assert_eq!(parsed.len(), 1);
    assert!(matches!(parsed[0], ProviderEvent::Completed(_)));
}

#[test]
fn a_failed_status_or_an_error_object_is_a_failure_with_the_provider_error() {
    for body in [
        json!({"status": "failed", "error": {"code": "server_error", "message": "boom"},
               "usage": {"input_tokens": 9, "output_tokens": 0}}),
        json!({"error": {"code": "server_error", "message": "boom"}}),
    ] {
        let parsed = parse_response_body(body.to_string().as_bytes()).unwrap();
        assert_eq!(parsed.len(), 1);
        let ProviderEvent::Failed(failure) = &parsed[0] else {
            panic!("{:?}", parsed[0]);
        };
        assert_eq!(failure.code.as_deref(), Some("server_error"));
        assert_eq!(failure.message, "boom");
    }
}

#[test]
fn a_body_that_is_not_a_json_object_is_a_provider_error() {
    for raw in [&b"not json"[..], b"[1,2]", b"\"text\"", b""] {
        let failure = parse_response_body(raw).unwrap_err();
        assert_eq!(failure.kind, ProviderErrorKind::Error);
        assert!(!failure.context_length_exceeded);
    }
}

// ── dispatch on the response content type ────────────────────────────────────

#[tokio::test]
async fn a_json_answer_to_the_summary_request_becomes_text_and_completion() {
    let oagw = FakeOagw::new();
    oagw.push(ProviderReply::json_text(
        "<summary>ok</summary>",
        &Opts::default().usage(900, 120),
    ));
    let got = events(run(&oagw, &request(true)).await.unwrap());
    assert_eq!(
        got[0],
        ProviderEvent::TextDelta("<summary>ok</summary>".to_owned())
    );
    let ProviderEvent::Completed(completion) = &got[1] else {
        panic!("{:?}", got[1]);
    };
    assert_eq!(
        (completion.input_tokens, completion.output_tokens),
        (900, 120)
    );
    assert_eq!(got.len(), 2);
}

#[tokio::test]
async fn an_event_stream_answer_to_the_summary_request_is_still_accepted() {
    let oagw = FakeOagw::new();
    oagw.push(ProviderReply::text(&["a ", "streamed ", "summary"]));
    let got = events(run(&oagw, &request(true)).await.unwrap());
    let text: String = got
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::TextDelta(delta) => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "a streamed summary");
    assert!(matches!(got.last(), Some(ProviderEvent::Completed(_))));
}

#[tokio::test]
async fn a_chat_request_still_refuses_a_json_answer() {
    let oagw = FakeOagw::new();
    oagw.push(ProviderReply::json_text("not a stream", &Opts::default()));
    let failure = failure_of(run(&oagw, &request(false)).await);
    assert_eq!(failure.kind, ProviderErrorKind::Error);
}

#[tokio::test]
async fn an_unparseable_json_answer_to_the_summary_request_is_a_provider_error() {
    let oagw = FakeOagw::new();
    oagw.push(ProviderReply::Json(Value::String("nope".to_owned())));
    let failure = failure_of(run(&oagw, &request(true)).await);
    assert_eq!(failure.kind, ProviderErrorKind::Error);
}

// ── errors keep their mapping ────────────────────────────────────────────────

#[tokio::test]
async fn http_400_context_length_exceeded_is_flagged_for_the_summary_call() {
    let oagw = FakeOagw::new();
    oagw.push(ProviderReply::Http {
        status: 400,
        body: json!({"error": {"code": "context_length_exceeded",
                               "message": "too long for gpt-5 (org-123)"}}),
        headers: vec![],
    });
    let failure = failure_of(run(&oagw, &request(true)).await);
    assert!(failure.context_length_exceeded);
    assert_eq!(failure.kind, ProviderErrorKind::Error);
}

#[tokio::test]
async fn other_http_errors_are_sanitized_provider_errors_for_the_summary_call() {
    let oagw = FakeOagw::new();
    oagw.push(ProviderReply::http_error(500, "summary provider down"));
    oagw.push(ProviderReply::http_error(429, "slow down"));
    let failure = failure_of(run(&oagw, &request(true)).await);
    assert!(!failure.context_length_exceeded);
    assert_eq!(failure.kind, ProviderErrorKind::Error);
    assert!(failure.message.contains("summary provider down"));
    let limited = failure_of(run(&oagw, &request(true)).await);
    assert_eq!(limited.kind, ProviderErrorKind::RateLimited);
}
