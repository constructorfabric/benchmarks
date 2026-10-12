#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::StreamExt;
use mini_chat_sdk::ModelApiParams;
use oagw_sdk::api::ErrorSource;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    AnthropicMessagesAdapter, MessagesTranslator, build_request_body, incomplete_reason,
    normalize_usage, parse_completion, request_headers,
};
use crate::config::ProviderKind;
use crate::domain::llm::{
    LlmAnnotation, LlmAnnotationKind, LlmEvent, LlmFailure, LlmFailureKind, LlmFunctionCall,
    LlmFunctionTool, LlmInputItem, LlmMetadata, LlmProvider, LlmRequest, LlmRequestType, LlmTool,
    LlmUsage, ProviderTarget,
};
use crate::infra::llm::fake_gateway::{
    FakeGateway, StaticSystemContext, json_response, lock, sse_response,
};
use crate::infra::llm::sse::{SseParser, SseTranslator};

const RAW_MESSAGE: &str = "Overloaded while serving msg_AbCdEf1234567890 \
    with file_AbCdEf123456789012 and key sk-AbCdEfGhIjKlMnOp0123 at https://internal-llm.example.com/v1";

// ── Request building ─────────────────────────────────────────────────────

fn request(tools: Vec<LlmTool>, api_params: ModelApiParams) -> LlmRequest {
    LlmRequest {
        model: "claude-sonnet-4-5".to_owned(),
        instructions: "Be brief.".to_owned(),
        input: vec![
            LlmInputItem::User {
                text: "first".to_owned(),
                image_file_ids: vec![],
            },
            LlmInputItem::Assistant {
                text: "answer".to_owned(),
            },
            LlmInputItem::User {
                text: "What is in these photos?".to_owned(),
                image_file_ids: vec!["file-primary1".to_owned(), "file-primary2".to_owned()],
            },
        ],
        tools,
        max_output_tokens: 8192,
        max_tool_calls: 2,
        api_params,
        user: "u".repeat(64),
        metadata: LlmMetadata {
            tenant_id: "t".to_owned(),
            user_id: "u".to_owned(),
            chat_id: "c".to_owned(),
            request_type: LlmRequestType::Chat,
            feature: "none".to_owned(),
        },
        secondary_file_ids: BTreeMap::from([(
            "file-primary1".to_owned(),
            "file_secondary1".to_owned(),
        )]),
    }
}

fn all_tools() -> Vec<LlmTool> {
    vec![
        LlmTool::FileSearch {
            vector_store_id: "vs_1".to_owned(),
            max_num_results: 5,
        },
        LlmTool::WebSearch {
            search_context_size: "low".to_owned(),
        },
        LlmTool::CodeInterpreter {
            file_ids: vec!["file-x".to_owned()],
        },
        LlmTool::Function(LlmFunctionTool {
            name: "search_knowledge".to_owned(),
            description: "Search".to_owned(),
            parameters: json!({ "type": "object" }),
        }),
    ]
}

#[test]
fn request_body_shape() {
    let extra: Map<String, Value> = json!({ "top_k": 7, "service_tier": "standard_only" })
        .as_object()
        .unwrap()
        .clone();
    let params = ModelApiParams {
        temperature: Some(0.5),
        frequency_penalty: Some(0.1),
        stop: vec!["END".to_owned()],
        extra_body: Some(extra),
        ..ModelApiParams::default()
    };
    let body = build_request_body(&request(vec![], params), true);
    assert_eq!(
        body,
        json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 8192,
            "stream": true,
            "system": "Be brief.",
            "messages": [
                { "role": "user", "content": [{ "type": "text", "text": "first" }] },
                { "role": "assistant", "content": [{ "type": "text", "text": "answer" }] },
                { "role": "user", "content": [
                    { "type": "image", "source": { "type": "file", "file_id": "file_secondary1" } },
                    { "type": "text", "text": "What is in these photos?" },
                ] },
            ],
            "metadata": { "user_id": "u".repeat(64) },
            "temperature": 0.5,
            "stop_sequences": ["END"],
        }),
        "no extra_body, no user, no max_tool_calls, unsupported params dropped, image without a copy dropped"
    );
}

#[test]
fn consecutive_roles_are_merged_and_empty_texts_dropped() {
    let mut req = request(vec![], ModelApiParams::default());
    req.instructions = String::new();
    req.input = vec![
        LlmInputItem::User {
            text: "summary".to_owned(),
            image_file_ids: vec![],
        },
        LlmInputItem::User {
            text: "question".to_owned(),
            image_file_ids: vec![],
        },
        LlmInputItem::Assistant {
            text: String::new(),
        },
        LlmInputItem::User {
            text: "again".to_owned(),
            image_file_ids: vec![],
        },
    ];
    let body = build_request_body(&req, false);
    assert!(body.get("system").is_none());
    assert_eq!(
        body["messages"],
        json!([{ "role": "user", "content": [
            { "type": "text", "text": "summary" },
            { "type": "text", "text": "question" },
            { "type": "text", "text": "again" },
        ] }])
    );
}

#[test]
fn agentic_input_sends_tool_use_and_tool_result_blocks() {
    let mut req = request(vec![], ModelApiParams::default());
    req.input.extend([
        LlmInputItem::Assistant {
            text: "Let me check.".to_owned(),
        },
        LlmInputItem::FunctionCall(LlmFunctionCall {
            call_id: "call_1".to_owned(),
            name: "search_knowledge".to_owned(),
            arguments: r#"{"query":"revenue"}"#.to_owned(),
        }),
        LlmInputItem::FunctionCall(LlmFunctionCall {
            call_id: "call_2".to_owned(),
            name: "search_knowledge".to_owned(),
            arguments: "not json".to_owned(),
        }),
        LlmInputItem::FunctionCallOutput {
            call_id: "call_1".to_owned(),
            output: r#"{"results":[]}"#.to_owned(),
        },
        LlmInputItem::FunctionCallOutput {
            call_id: "call_2".to_owned(),
            output: "search limit reached".to_owned(),
        },
    ]);
    let body = build_request_body(&req, true);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(
        messages[messages.len() - 2..],
        [
            json!({ "role": "assistant", "content": [
                { "type": "text", "text": "Let me check." },
                { "type": "tool_use", "id": "call_1", "name": "search_knowledge",
                  "input": { "query": "revenue" } },
                { "type": "tool_use", "id": "call_2", "name": "search_knowledge", "input": {} },
            ] }),
            json!({ "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": "call_1", "content": r#"{"results":[]}"# },
                { "type": "tool_result", "tool_use_id": "call_2", "content": "search limit reached" },
            ] }),
        ]
    );
}

#[test]
fn tools_map_to_server_tools_and_never_file_search() {
    let body = build_request_body(&request(all_tools(), ModelApiParams::default()), true);
    assert_eq!(
        body["tools"],
        json!([
            { "type": "web_search_20250305", "name": "web_search" },
            { "type": "code_execution_20250522", "name": "code_execution" },
            { "name": "search_knowledge", "description": "Search", "input_schema": { "type": "object" } },
        ])
    );
    assert!(!body.to_string().contains("vector_store"));
    let only_file_search = vec![LlmTool::FileSearch {
        vector_store_id: "vs_1".to_owned(),
        max_num_results: 5,
    }];
    let body = build_request_body(&request(only_file_search, ModelApiParams::default()), true);
    assert!(body.get("tools").is_none());
}

#[test]
fn headers_name_the_version_and_the_needed_betas() {
    let body = build_request_body(&request(all_tools(), ModelApiParams::default()), true);
    assert_eq!(
        request_headers(&body),
        vec![
            ("anthropic-version", "2023-06-01".to_owned()),
            (
                "anthropic-beta",
                "files-api-2025-04-14,code-execution-2025-05-22".to_owned()
            ),
        ]
    );
    let mut req = request(vec![], ModelApiParams::default());
    req.secondary_file_ids.clear();
    let body = build_request_body(&req, true);
    assert_eq!(
        request_headers(&body),
        vec![("anthropic-version", "2023-06-01".to_owned())]
    );
}

// ── Usage, stop reasons, completion ──────────────────────────────────────

#[test]
fn usage_counts_cache_tokens_as_input_subsets() {
    let usage = normalize_usage(&json!({
        "input_tokens": 50,
        "output_tokens": 9,
        "cache_read_input_tokens": 4000,
        "cache_creation_input_tokens": 100,
    }));
    assert_eq!(
        usage,
        LlmUsage {
            input_tokens: 4150,
            output_tokens: 9,
            cache_read_input_tokens: 4000,
            cache_write_input_tokens: 100,
            reasoning_tokens: 0,
        }
    );
}

#[test]
fn stop_reasons_map_to_incomplete_reasons() {
    assert_eq!(
        incomplete_reason("max_tokens").as_deref(),
        Some("max_tokens")
    );
    assert_eq!(
        incomplete_reason("model_context_window_exceeded").as_deref(),
        Some("max_tokens")
    );
    assert_eq!(
        incomplete_reason("refusal").as_deref(),
        Some("content_filter")
    );
    assert_eq!(incomplete_reason("pause_turn").as_deref(), Some("other"));
    assert_eq!(incomplete_reason("end_turn"), None);
    assert_eq!(incomplete_reason("tool_use"), None);
}

#[test]
fn completion_parsing() {
    let body = json!({
        "id": "msg_1", "type": "message", "role": "assistant",
        "content": [{ "type": "text", "text": "sum" }, { "type": "text", "text": "mary" }],
        "stop_reason": "end_turn",
        "usage": { "input_tokens": 10, "output_tokens": 2, "cache_read_input_tokens": 5 },
    });
    let completion = parse_completion(body.to_string().as_bytes()).unwrap();
    assert_eq!(completion.text, "summary");
    assert_eq!(completion.response_id.as_deref(), Some("msg_1"));
    assert_eq!(completion.usage.unwrap().input_tokens, 15);
    assert_eq!(completion.incomplete_reason, None);
    let failure = parse_completion(
        json!({ "type": "error", "error": { "type": "api_error", "message": RAW_MESSAGE } })
            .to_string()
            .as_bytes(),
    )
    .unwrap_err();
    assert_eq!(failure.provider_code.as_deref(), Some("api_error"));
    assert!(!failure.message_sanitized.contains("msg_AbCd"));
}

// ── Stream translation ───────────────────────────────────────────────────

fn sse(name: &str, data: &Value) -> String {
    format!("event: {name}\ndata: {data}\n\n")
}

fn message_start() -> String {
    sse(
        "message_start",
        &json!({ "type": "message_start", "message": {
            "id": "msg_internal1", "type": "message", "role": "assistant", "content": [],
            "usage": { "input_tokens": 50, "cache_creation_input_tokens": 100,
                       "cache_read_input_tokens": 4000, "output_tokens": 1 },
        }}),
    )
}

fn block_start(index: u64, block: &Value) -> String {
    sse(
        "content_block_start",
        &json!({ "type": "content_block_start", "index": index, "content_block": block }),
    )
}

fn block_delta(index: u64, delta: &Value) -> String {
    sse(
        "content_block_delta",
        &json!({ "type": "content_block_delta", "index": index, "delta": delta }),
    )
}

fn block_stop(index: u64) -> String {
    sse(
        "content_block_stop",
        &json!({ "type": "content_block_stop", "index": index }),
    )
}

fn message_end(stop_reason: &str) -> String {
    [
        sse(
            "message_delta",
            &json!({ "type": "message_delta", "delta": { "stop_reason": stop_reason },
                     "usage": { "output_tokens": 9 } }),
        ),
        sse("message_stop", &json!({ "type": "message_stop" })),
    ]
    .concat()
}

fn translate(wire: &str) -> Vec<LlmEvent> {
    let mut parser = SseParser::default();
    let mut translator = MessagesTranslator::default();
    let mut out = Vec::new();
    for event in parser.push(wire.as_bytes()) {
        translator.translate(event, &mut out);
    }
    for event in parser.finish() {
        translator.translate(event, &mut out);
    }
    out
}

fn expected_usage() -> LlmUsage {
    LlmUsage {
        input_tokens: 4150,
        output_tokens: 9,
        cache_read_input_tokens: 4000,
        cache_write_input_tokens: 100,
        reasoning_tokens: 0,
    }
}

#[test]
fn text_stream_with_cache_usage() {
    let wire = [
        message_start(),
        sse("ping", &json!({ "type": "ping" })),
        block_start(0, &json!({ "type": "text", "text": "" })),
        block_delta(0, &json!({ "type": "text_delta", "text": "Hello " })),
        block_delta(0, &json!({ "type": "text_delta", "text": "Claude." })),
        block_stop(0),
        message_end("end_turn"),
    ]
    .concat();
    assert_eq!(
        translate(&wire),
        vec![
            LlmEvent::TextDelta("Hello ".to_owned()),
            LlmEvent::TextDelta("Claude.".to_owned()),
            LlmEvent::Completed {
                response_id: Some("msg_internal1".to_owned()),
                usage: Some(expected_usage()),
                incomplete_reason: None,
                output_text: "Hello Claude.".to_owned(),
            },
        ]
    );
}

#[test]
fn max_tokens_stop_completes_as_incomplete() {
    let wire = [
        message_start(),
        block_start(0, &json!({ "type": "text", "text": "" })),
        block_delta(0, &json!({ "type": "text_delta", "text": "Cut off mid" })),
        block_stop(0),
        message_end("max_tokens"),
    ]
    .concat();
    let events = translate(&wire);
    let Some(LlmEvent::Completed {
        incomplete_reason, ..
    }) = events.last()
    else {
        panic!("{events:?}");
    };
    assert_eq!(incomplete_reason.as_deref(), Some("max_tokens"));
}

#[test]
fn server_tools_map_to_shared_tool_events_and_citations() {
    let wire = [
        message_start(),
        block_start(
            0,
            &json!({ "type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {} }),
        ),
        block_delta(
            0,
            &json!({ "type": "input_json_delta", "partial_json": "{\"query\":\"news\"}" }),
        ),
        block_stop(0),
        block_start(
            1,
            &json!({ "type": "web_search_tool_result", "tool_use_id": "srvtoolu_1", "content": [] }),
        ),
        block_stop(1),
        block_start(
            2,
            &json!({ "type": "server_tool_use", "id": "srvtoolu_2", "name": "code_execution", "input": {} }),
        ),
        block_stop(2),
        block_start(
            3,
            &json!({ "type": "code_execution_tool_result", "tool_use_id": "srvtoolu_2", "content": {} }),
        ),
        block_stop(3),
        block_start(4, &json!({ "type": "text", "text": "" })),
        block_delta(
            4,
            &json!({ "type": "citations_delta", "citation": {
                "type": "web_search_result_location", "url": "https://example.com/a",
                "title": "A", "cited_text": "fresh", "encrypted_index": "x" } }),
        ),
        block_delta(4, &json!({ "type": "text_delta", "text": "Fresh news." })),
        block_stop(4),
        message_end("end_turn"),
    ]
    .concat();
    let events = translate(&wire);
    let tool = |name: &str, done: bool| {
        if done {
            LlmEvent::ToolCompleted {
                name: name.to_owned(),
                details: json!({}),
            }
        } else {
            LlmEvent::ToolStarted {
                name: name.to_owned(),
                details: json!({}),
            }
        }
    };
    assert_eq!(
        events[..5],
        [
            tool("web_search", false),
            tool("web_search", true),
            tool("code_interpreter", false),
            tool("code_interpreter", true),
            LlmEvent::Annotation(LlmAnnotation {
                kind: LlmAnnotationKind::Url,
                url: Some("https://example.com/a".to_owned()),
                title: Some("A".to_owned()),
                file_id: None,
                filename: None,
                start: None,
                end: None,
                snippet: "fresh".to_owned(),
            }),
        ]
    );
    assert_eq!(events[5], LlmEvent::TextDelta("Fresh news.".to_owned()));
}

#[test]
fn client_tool_use_starts_a_named_tool_event_and_a_function_call() {
    let wire = [
        message_start(),
        block_start(
            0,
            &json!({ "type": "tool_use", "id": "toolu_1", "name": "search_knowledge", "input": {} }),
        ),
        block_delta(
            0,
            &json!({ "type": "input_json_delta", "partial_json": "{\"query\"" }),
        ),
        block_delta(0, &json!({ "type": "input_json_delta", "partial_json": ":\"x\"}" })),
        block_stop(0),
        block_start(
            1,
            &json!({ "type": "tool_use", "id": "toolu_2", "name": "other_tool", "input": {} }),
        ),
        block_stop(1),
        message_end("tool_use"),
    ]
    .concat();
    let events = translate(&wire);
    assert_eq!(
        events[..4],
        [
            LlmEvent::ToolStarted {
                name: "search_knowledge".to_owned(),
                details: json!({}),
            },
            LlmEvent::FunctionCall(LlmFunctionCall {
                call_id: "toolu_1".to_owned(),
                name: "search_knowledge".to_owned(),
                arguments: "{\"query\":\"x\"}".to_owned(),
            }),
            LlmEvent::ToolStarted {
                name: "unknown_tool".to_owned(),
                details: json!({}),
            },
            LlmEvent::FunctionCall(LlmFunctionCall {
                call_id: "toolu_2".to_owned(),
                name: "other_tool".to_owned(),
                arguments: "{}".to_owned(),
            }),
        ]
    );
    assert!(matches!(
        events[4],
        LlmEvent::Completed {
            incomplete_reason: None,
            ..
        }
    ));
}

#[test]
fn error_event_is_a_sanitized_provider_error() {
    let wire = [
        message_start(),
        sse(
            "error",
            &json!({ "type": "error", "error": { "type": "overloaded_error", "message": RAW_MESSAGE } }),
        ),
    ]
    .concat();
    let events = translate(&wire);
    let [LlmEvent::Failed(failure)] = events.as_slice() else {
        panic!("{events:?}");
    };
    assert_eq!(failure.kind, LlmFailureKind::ProviderError);
    assert_eq!(failure.provider_code.as_deref(), Some("overloaded_error"));
    for leaked in ["msg_AbCd", "file_AbCd", "sk-AbCd", "https://internal-llm"] {
        assert!(!failure.message_sanitized.contains(leaked), "{failure:?}");
    }
}

#[test]
fn end_of_body_without_stop_reason_is_a_failure() {
    let mut translator = MessagesTranslator::default();
    assert_eq!(
        translator.stream_ended(),
        LlmEvent::Failed(LlmFailure::provider_error(
            "provider stream ended unexpectedly"
        ))
    );
}

// ── Adapter over a fake gateway ──────────────────────────────────────────

fn target() -> ProviderTarget {
    ProviderTarget {
        provider_id: "anthropic".to_owned(),
        kind: ProviderKind::AnthropicMessages,
        alias: "mock-anthropic".to_owned(),
        api_path: "/v1/messages".to_owned(),
        tenant_id: Uuid::from_u128(7),
        tenant_override: false,
    }
}

fn adapter(gateway: &Arc<FakeGateway>) -> AnthropicMessagesAdapter {
    AnthropicMessagesAdapter::new(gateway.clone(), Arc::new(StaticSystemContext::default()))
}

#[tokio::test]
async fn stream_sends_the_version_header_through_oagw() {
    let gateway = Arc::new(FakeGateway::default());
    let wire = [
        message_start(),
        block_start(0, &json!({ "type": "text", "text": "" })),
        block_delta(0, &json!({ "type": "text_delta", "text": "Hi" })),
        block_stop(0),
        message_end("end_turn"),
    ]
    .concat();
    gateway.push_proxy(Ok(sse_response(vec![wire])));
    let events: Vec<LlmEvent> = adapter(&gateway)
        .stream(
            &target(),
            request(vec![], ModelApiParams::default()),
            CancellationToken::new(),
        )
        .await
        .unwrap()
        .collect()
        .await;
    assert_eq!(events[0], LlmEvent::TextDelta("Hi".to_owned()));
    let sent = lock(&gateway.proxied)[0].clone();
    assert_eq!(sent.uri, "/mock-anthropic/v1/messages");
    assert_eq!(sent.headers["anthropic-version"], "2023-06-01");
    assert_eq!(sent.headers["anthropic-beta"], "files-api-2025-04-14");
    assert_eq!(sent.body["metadata"]["user_id"], json!("u".repeat(64)));
}

#[tokio::test]
async fn http_429_is_rate_limited() {
    let gateway = Arc::new(FakeGateway::default());
    gateway.push_proxy(Ok(json_response(
        429,
        &json!({ "type": "error", "error": { "type": "rate_limit_error", "message": RAW_MESSAGE } }),
        &[("retry-after", "23")],
        Some(ErrorSource::Upstream),
    )));
    let Err(failure) = adapter(&gateway)
        .stream(
            &target(),
            request(vec![], ModelApiParams::default()),
            CancellationToken::new(),
        )
        .await
    else {
        panic!("expected a failure");
    };
    assert_eq!(failure.kind, LlmFailureKind::RateLimited);
    assert_eq!(failure.retry_after_secs, Some(23));
}

#[tokio::test]
async fn complete_sends_a_non_streaming_request() {
    let gateway = Arc::new(FakeGateway::default());
    gateway.push_proxy(Ok(json_response(
        200,
        &json!({ "id": "msg_s", "type": "message", "content": [{ "type": "text", "text": "summary" }],
                 "stop_reason": "end_turn", "usage": { "input_tokens": 3, "output_tokens": 1 } }),
        &[],
        None,
    )));
    let completion = adapter(&gateway)
        .complete(&target(), request(vec![], ModelApiParams::default()))
        .await
        .unwrap();
    assert_eq!(completion.text, "summary");
    let sent = lock(&gateway.proxied)[0].clone();
    assert_eq!(sent.body["stream"], json!(false));
}
