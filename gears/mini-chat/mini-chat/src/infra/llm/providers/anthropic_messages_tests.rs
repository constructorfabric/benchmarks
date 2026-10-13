#![allow(clippy::unwrap_used, clippy::expect_used)]

use futures::StreamExt;
use mini_chat_sdk::{ModelApiParams, UsageTokens};
use oagw_sdk::api::ErrorSource;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::config::ProviderKind;
use crate::infra::llm::fake_gw::{FakeGw, Reply, client, http_error, json_ok, sse};
use crate::infra::llm::types::{
    ContentPart, InputItem, LlmClient, LlmEvent, LlmRequest, ProviderError, RawCitation,
    RequestMetadata, ResolvedProvider, ToolSpec, provider_user,
};

const TENANT: &str = "6f1d7a52-0f6e-4c37-9a3c-0d7a8f3c2b11";
const USER: &str = "0b9e4c1d-2f3a-4b5c-8d6e-7f8091a2b3c4";

fn provider() -> ResolvedProvider {
    ResolvedProvider {
        provider_id: "anthropic".to_owned(),
        kind: ProviderKind::AnthropicMessages,
        alias: "api.anthropic.com".to_owned(),
        api_path: "/v1/messages".to_owned(),
        storage: None,
    }
}

fn request() -> LlmRequest {
    LlmRequest {
        model: "claude-sonnet-4-6".to_owned(),
        instructions: "be helpful".to_owned(),
        input: vec![
            InputItem::Message {
                role: "user",
                content: vec![
                    ContentPart::InputText("what is this?".to_owned()),
                    ContentPart::InputImage {
                        file_id: "file-abc123".to_owned(),
                    },
                ],
            },
            InputItem::Message {
                role: "assistant",
                content: vec![ContentPart::OutputText("a cat".to_owned())],
            },
        ],
        tools: vec![],
        max_output_tokens: 1024,
        api_params: ModelApiParams::default(),
        max_tool_calls: Some(2),
        user: provider_user(TENANT, USER),
        metadata: RequestMetadata {
            tenant_id: TENANT.to_owned(),
            user_id: USER.to_owned(),
            chat_id: "c0ffee00-0000-4000-8000-000000000001".to_owned(),
            request_type: "chat",
            feature: "none".to_owned(),
        },
        stream: false,
    }
}

async fn run(reply: Reply) -> Vec<LlmEvent> {
    let gw = FakeGw::with(reply);
    let (client, _) = client(&gw);
    let stream = client
        .stream(&provider(), request(), CancellationToken::new())
        .await
        .unwrap();
    stream.collect().await
}

fn message_start() -> (&'static str, Value) {
    (
        "message_start",
        json!({"type": "message_start", "message": {
            "id": "msg_01", "type": "message", "role": "assistant", "content": [],
            "usage": {"input_tokens": 20, "cache_read_input_tokens": 100,
                      "cache_creation_input_tokens": 5, "output_tokens": 1}
        }}),
    )
}

fn block_start(index: u64, block: &Value) -> (&'static str, Value) {
    (
        "content_block_start",
        json!({"type": "content_block_start", "index": index, "content_block": block}),
    )
}

fn block_delta(index: u64, delta: &Value) -> (&'static str, Value) {
    (
        "content_block_delta",
        json!({"type": "content_block_delta", "index": index, "delta": delta}),
    )
}

fn text(index: u64, t: &str) -> (&'static str, Value) {
    block_delta(index, &json!({"type": "text_delta", "text": t}))
}

fn block_stop(index: u64) -> (&'static str, Value) {
    (
        "content_block_stop",
        json!({"type": "content_block_stop", "index": index}),
    )
}

fn message_delta(stop_reason: &str) -> (&'static str, Value) {
    (
        "message_delta",
        json!({"type": "message_delta", "delta": {"stop_reason": stop_reason, "stop_sequence": null},
               "usage": {"output_tokens": 40}}),
    )
}

fn message_stop() -> (&'static str, Value) {
    ("message_stop", json!({"type": "message_stop"}))
}

fn usage() -> UsageTokens {
    UsageTokens {
        input_tokens: 125,
        output_tokens: 40,
        cache_read_input_tokens: 100,
        cache_write_input_tokens: 5,
        reasoning_tokens: 0,
    }
}

fn completed(incomplete_reason: Option<&str>) -> LlmEvent {
    LlmEvent::Completed {
        usage: Some(usage()),
        response_id: Some("msg_01".to_owned()),
        incomplete_reason: incomplete_reason.map(str::to_owned),
    }
}

fn start(name: &str) -> LlmEvent {
    LlmEvent::ToolStart {
        name: name.to_owned(),
        details: json!({}),
    }
}

fn done(name: &str) -> LlmEvent {
    LlmEvent::ToolDone {
        name: name.to_owned(),
        details: json!({}),
    }
}

// ── Request ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn builds_messages_request() {
    let gw = FakeGw::with(sse(&[message_start(), message_stop()]));
    let (client, _) = client(&gw);
    let mut req = request();
    req.tools = vec![
        ToolSpec::FileSearch {
            vector_store_ids: vec!["vs_abc".to_owned()],
            max_num_results: 5,
        },
        ToolSpec::WebSearch {
            search_context_size: "low".to_owned(),
        },
        ToolSpec::CodeInterpreter {
            file_ids: vec!["file-x".to_owned()],
        },
        ToolSpec::Function {
            name: "search_knowledge".to_owned(),
            description: "kb".to_owned(),
            parameters: json!({"type": "object"}),
        },
    ];
    req.input.push(InputItem::FunctionCall {
        call_id: "toolu_1".to_owned(),
        name: "search_knowledge".to_owned(),
        arguments: r#"{"query":"a"}"#.to_owned(),
    });
    req.input.push(InputItem::FunctionCallOutput {
        call_id: "toolu_1".to_owned(),
        output: "A".to_owned(),
    });
    req.api_params.temperature = Some(0.5);
    req.api_params.stop = vec!["END".to_owned()];
    req.api_params.frequency_penalty = Some(1.0);
    req.api_params.extra_body = Some(json!({"top_k": 3}).as_object().unwrap().clone());
    let stream = client
        .stream(&provider(), req, CancellationToken::new())
        .await
        .unwrap();
    let _: Vec<_> = stream.collect().await;

    let cap = gw.last();
    assert_eq!(cap.uri, "/api.anthropic.com/v1/messages");
    assert_eq!(cap.header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(
        cap.header("anthropic-beta"),
        Some("code-execution-2025-08-25")
    );
    let body = cap.body;
    assert_eq!(body["model"], "claude-sonnet-4-6");
    assert_eq!(body["system"], "be helpful");
    assert_eq!(body["max_tokens"], 1024);
    assert_eq!(body["stream"], true);
    assert_eq!(
        body["metadata"],
        json!({"user_id": "6f1d7a520f6e4c379a3c0d7a8f3c2b110b9e4c1d2f3a4b5c8d6e7f8091a2b3c4"})
    );
    assert!(body.get("user").is_none());
    assert!(body.get("max_tool_calls").is_none());
    assert_eq!(
        body["messages"],
        json!([
            {"role": "user", "content": [{"type": "text", "text": "what is this?"}]},
            {"role": "assistant", "content": [{"type": "text", "text": "a cat"}]},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "search_knowledge", "input": {"query": "a"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "A"}
            ]}
        ])
    );
    assert_eq!(
        body["tools"],
        json!([
            {"type": "web_search_20250305", "name": "web_search"},
            {"type": "code_execution_20250825", "name": "code_execution"},
            {"name": "search_knowledge", "description": "kb", "input_schema": {"type": "object"}}
        ])
    );
    assert_eq!(body["temperature"], 0.5);
    assert_eq!(body["stop_sequences"], json!(["END"]));
    assert!(body.get("frequency_penalty").is_none());
    assert!(body.get("top_k").is_none(), "extra_body is not sent");
}

#[tokio::test]
async fn no_beta_header_without_code_execution() {
    let gw = FakeGw::with(sse(&[message_start(), message_stop()]));
    let (client, _) = client(&gw);
    let mut req = request();
    req.instructions = String::new();
    let stream = client
        .stream(&provider(), req, CancellationToken::new())
        .await
        .unwrap();
    let _: Vec<_> = stream.collect().await;
    let cap = gw.last();
    assert_eq!(cap.header("anthropic-beta"), None);
    assert!(cap.body.get("system").is_none());
    assert!(cap.body.get("tools").is_none());
}

// ── Streaming ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn streams_text_and_normalized_usage() {
    let events = run(sse(&[
        message_start(),
        block_start(0, &json!({"type": "text", "text": ""})),
        ("ping", json!({"type": "ping"})),
        text(0, "Hel"),
        text(0, "lo"),
        block_stop(0),
        message_delta("end_turn"),
        message_stop(),
    ]))
    .await;
    assert_eq!(
        events,
        vec![
            LlmEvent::TextDelta("Hel".to_owned()),
            LlmEvent::TextDelta("lo".to_owned()),
            completed(None),
        ]
    );
}

#[tokio::test]
async fn server_and_function_tools_events() {
    let events = run(sse(&[
        message_start(),
        block_start(0, &json!({"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {}})),
        block_stop(0),
        block_start(1, &json!({"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1", "content": []})),
        block_stop(1),
        block_start(2, &json!({"type": "text", "text": ""})),
        block_delta(2, &json!({"type": "citations_delta", "citation": {
            "type": "web_search_result_location", "url": "https://ex.com/a",
            "title": "A", "cited_text": "alpha", "encrypted_index": "xx"
        }})),
        text(2, "See A."),
        block_stop(2),
        block_start(3, &json!({"type": "server_tool_use", "id": "srvtoolu_2", "name": "bash_code_execution", "input": {}})),
        block_stop(3),
        block_start(4, &json!({"type": "bash_code_execution_tool_result", "tool_use_id": "srvtoolu_2", "content": {}})),
        block_stop(4),
        block_start(5, &json!({"type": "tool_use", "id": "toolu_9", "name": "search_knowledge", "input": {}})),
        block_delta(5, &json!({"type": "input_json_delta", "partial_json": "{\"query\":"})),
        block_delta(5, &json!({"type": "input_json_delta", "partial_json": "\"x\"}"})),
        block_stop(5),
        block_start(6, &json!({"type": "tool_use", "id": "toolu_10", "name": "drop_tables", "input": {}})),
        block_stop(6),
        message_delta("tool_use"),
        message_stop(),
    ]))
    .await;
    assert_eq!(
        events,
        vec![
            start("web_search"),
            done("web_search"),
            LlmEvent::TextDelta("See A.".to_owned()),
            start("code_interpreter"),
            done("code_interpreter"),
            start("search_knowledge"),
            LlmEvent::FunctionCall {
                call_id: "toolu_9".to_owned(),
                name: "search_knowledge".to_owned(),
                arguments: "{\"query\":\"x\"}".to_owned(),
            },
            start("unknown_tool"),
            LlmEvent::FunctionCall {
                call_id: "toolu_10".to_owned(),
                name: "drop_tables".to_owned(),
                arguments: String::new(),
            },
            LlmEvent::Citations(vec![RawCitation::Web {
                url: "https://ex.com/a".to_owned(),
                title: "A".to_owned(),
                snippet: "alpha".to_owned(),
                span: None,
            }]),
            completed(None),
        ]
    );
}

#[tokio::test]
async fn max_tokens_stop_reason_is_incomplete() {
    let events = run(sse(&[
        message_start(),
        text(0, "trunc"),
        message_delta("max_tokens"),
        message_stop(),
    ]))
    .await;
    assert_eq!(events.last().unwrap(), &completed(Some("max_tokens")));
}

// ── Errors ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn error_event_is_failed() {
    let events = run(sse(&[
        message_start(),
        (
            "error",
            json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
        ),
    ]))
    .await;
    assert_eq!(
        events,
        vec![LlmEvent::Failed {
            error: ProviderError::provider("Overloaded"),
            usage: None,
        }]
    );
}

#[tokio::test]
async fn http_error_body_is_mapped_and_sanitized() {
    let gw = FakeGw::with(http_error(
        400,
        ErrorSource::Upstream,
        vec![],
        &json!({"type": "error", "error": {"type": "invalid_request_error", "message": "bad file file-abcdefghijklmnopqrst"}}),
    ));
    let (client, _) = client(&gw);
    let Err(err) = client
        .stream(&provider(), request(), CancellationToken::new())
        .await
    else {
        panic!("expected an error");
    };
    assert_eq!(err.code, "provider_error");
    assert_eq!(err.message, "bad file [provider_id]");
}

// ── Non-streaming ────────────────────────────────────────────────────────────

#[tokio::test]
async fn complete_returns_text_and_usage() {
    let gw = FakeGw::with(json_ok(&json!({
        "id": "msg_02", "type": "message", "role": "assistant", "stop_reason": "end_turn",
        "content": [{"type": "text", "text": "Sum"}, {"type": "text", "text": "mary"}],
        "usage": {"input_tokens": 20, "cache_read_input_tokens": 100,
                  "cache_creation_input_tokens": 5, "output_tokens": 40}
    })));
    let (client, _) = client(&gw);
    let out = client.complete(&provider(), request()).await.unwrap();
    assert_eq!(out.text, "Summary");
    assert_eq!(out.usage, Some(usage()));
    assert_eq!(gw.last().body["stream"], false);
}
