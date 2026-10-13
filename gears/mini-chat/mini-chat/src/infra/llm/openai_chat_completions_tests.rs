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
    ChatCompletionsTranslator, OpenAiChatCompletionsAdapter, TOOL_FUNCTION_CALL,
    build_request_body, incomplete_reason, parse_completion, parse_usage,
};
use crate::config::ProviderKind;
use crate::domain::llm::{
    LlmEvent, LlmFailure, LlmFailureKind, LlmFunctionCall, LlmFunctionTool, LlmInputItem,
    LlmMetadata, LlmProvider, LlmRequest, LlmRequestType, LlmTool, LlmUsage, ProviderTarget,
};
use crate::infra::llm::fake_gateway::{
    FakeGateway, StaticSystemContext, json_response, lock, sse_response,
};
use crate::infra::llm::sse::{SseParser, SseTranslator};

const RAW_MESSAGE: &str = "Upstream failure for response chatcmpl-abc123def456ghi789 \
    reading file-AbCdEf123456789012 with key sk-AbCdEfGhIjKlMnOp0123 at https://internal-llm.example.com/v1";

// ── Request building ─────────────────────────────────────────────────────

fn request(tools: Vec<LlmTool>, api_params: ModelApiParams) -> LlmRequest {
    LlmRequest {
        model: "gpt-4o-2024-08-06".to_owned(),
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
                text: "second".to_owned(),
                image_file_ids: vec!["file-img1".to_owned()],
            },
        ],
        tools,
        max_output_tokens: 4096,
        max_tool_calls: 2,
        api_params,
        user: "u".repeat(64),
        metadata: LlmMetadata {
            tenant_id: "t".to_owned(),
            user_id: "u".to_owned(),
            chat_id: "c".to_owned(),
            request_type: LlmRequestType::Chat,
            feature: "web_search".to_owned(),
        },
        secondary_file_ids: BTreeMap::new(),
    }
}

fn function_tool() -> LlmTool {
    LlmTool::Function(LlmFunctionTool {
        name: "search_knowledge".to_owned(),
        description: "Search the knowledge base".to_owned(),
        parameters: json!({ "type": "object", "properties": { "query": { "type": "string" } } }),
    })
}

fn built_in_tools() -> Vec<LlmTool> {
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
    ]
}

#[test]
fn request_body_carries_system_history_user_identity_and_limit() {
    let body = build_request_body(&request(vec![], ModelApiParams::default()), true);
    assert_eq!(
        body,
        json!({
            "model": "gpt-4o-2024-08-06",
            "messages": [
                { "role": "system", "content": "Be brief." },
                { "role": "user", "content": "first" },
                { "role": "assistant", "content": "answer" },
                { "role": "user", "content": "second" },
            ],
            "stream": true,
            "stream_options": { "include_usage": true },
            "max_completion_tokens": 4096,
            "user": "u".repeat(64),
        })
    );
    let body = build_request_body(&request(vec![], ModelApiParams::default()), false);
    assert_eq!(body["stream"], json!(false));
    assert!(body.get("stream_options").is_none());
}

#[test]
fn agentic_input_joins_tool_calls_to_the_assistant_message() {
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
        messages[messages.len() - 3..],
        [
            json!({ "role": "assistant", "content": "Let me check.", "tool_calls": [
                { "id": "call_1", "type": "function",
                  "function": { "name": "search_knowledge", "arguments": r#"{"query":"revenue"}"# } },
                { "id": "call_2", "type": "function",
                  "function": { "name": "search_knowledge", "arguments": "not json" } },
            ] }),
            json!({ "role": "tool", "tool_call_id": "call_1", "content": r#"{"results":[]}"# }),
            json!({ "role": "tool", "tool_call_id": "call_2", "content": "search limit reached" }),
        ]
    );

    req.input.remove(req.input.len() - 5);
    let body = build_request_body(&req, true);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(
        messages[messages.len() - 3]["content"],
        Value::Null,
        "calls without text get their own assistant message"
    );
    assert_eq!(messages[messages.len() - 4]["role"], "user");
}

#[test]
fn request_body_keeps_function_tools_and_drops_built_in_tools() {
    let body = build_request_body(&request(built_in_tools(), ModelApiParams::default()), true);
    assert!(body.get("tools").is_none(), "{body}");

    let mut tools = built_in_tools();
    tools.push(function_tool());
    let body = build_request_body(&request(tools, ModelApiParams::default()), true);
    assert_eq!(
        body["tools"],
        json!([{
            "type": "function",
            "function": {
                "name": "search_knowledge",
                "description": "Search the knowledge base",
                "parameters": { "type": "object", "properties": { "query": { "type": "string" } } },
            },
        }])
    );
    assert!(body.get("metadata").is_none());
    assert!(body.get("max_tool_calls").is_none());
}

#[test]
fn request_body_merges_extra_body_without_controlled_keys() {
    let extra: Map<String, Value> = json!({
        "top_k": 40,
        "repetition_penalty": 1.05,
        "model": "hijacked",
        "user": "hijacked",
        "stream": false,
        "messages": [],
        "max_tool_calls": 99,
        "stream_options": {},
    })
    .as_object()
    .unwrap()
    .clone();
    let params = ModelApiParams {
        temperature: Some(0.2),
        stop: vec!["END".to_owned()],
        reasoning_effort: Some("low".to_owned()),
        extra_body: Some(extra),
        ..ModelApiParams::default()
    };
    let body = build_request_body(&request(vec![], params), true);
    assert_eq!(body["top_k"], json!(40));
    assert_eq!(body["repetition_penalty"], json!(1.05));
    assert_eq!(body["model"], json!("gpt-4o-2024-08-06"));
    assert_eq!(body["user"], json!("u".repeat(64)));
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["stream_options"], json!({ "include_usage": true }));
    assert_eq!(body["messages"].as_array().unwrap().len(), 4);
    assert!(body.get("max_tool_calls").is_none());
    assert_eq!(body["temperature"], json!(0.2));
    assert_eq!(body["stop"], json!(["END"]));
    assert_eq!(body["reasoning_effort"], json!("low"));
}

// ── Usage and completion ─────────────────────────────────────────────────

#[test]
fn usage_is_normalized_from_prompt_and_completion_tokens() {
    let usage = parse_usage(&json!({ "usage": {
        "prompt_tokens": 321,
        "completion_tokens": 45,
        "total_tokens": 366,
        "prompt_tokens_details": { "cached_tokens": 100 },
        "completion_tokens_details": { "reasoning_tokens": 7 },
    }}));
    assert_eq!(
        usage,
        Some(LlmUsage {
            input_tokens: 321,
            output_tokens: 45,
            cache_read_input_tokens: 100,
            cache_write_input_tokens: 0,
            reasoning_tokens: 7,
        })
    );
    assert_eq!(
        parse_usage(&json!({ "usage": { "prompt_tokens": 5 } })),
        Some(LlmUsage {
            input_tokens: 5,
            ..LlmUsage::default()
        }),
        "missing fields are 0"
    );
    assert_eq!(parse_usage(&json!({ "usage": null })), None);
}

#[test]
fn finish_reasons_map_to_incomplete_reasons() {
    assert_eq!(incomplete_reason("length").as_deref(), Some("max_tokens"));
    assert_eq!(
        incomplete_reason("content_filter").as_deref(),
        Some("content_filter")
    );
    assert_eq!(incomplete_reason("stop"), None);
    assert_eq!(incomplete_reason("tool_calls"), None);
}

#[test]
fn completion_parsing() {
    let body = json!({
        "id": "chatcmpl-1",
        "choices": [{ "index": 0, "message": { "role": "assistant", "content": "summary" },
                      "finish_reason": "length" }],
        "usage": { "prompt_tokens": 10, "completion_tokens": 2 },
    });
    let completion = parse_completion(body.to_string().as_bytes()).unwrap();
    assert_eq!(completion.text, "summary");
    assert_eq!(completion.response_id.as_deref(), Some("chatcmpl-1"));
    assert_eq!(completion.incomplete_reason.as_deref(), Some("max_tokens"));
    assert_eq!(completion.usage.unwrap().input_tokens, 10);

    let failure = parse_completion(
        json!({ "error": { "message": RAW_MESSAGE } })
            .to_string()
            .as_bytes(),
    )
    .unwrap_err();
    assert_eq!(failure.kind, LlmFailureKind::ProviderError);
    assert!(!failure.message_sanitized.contains("chatcmpl-abc"));
    assert!(parse_completion(b"not json").is_err());
}

// ── Stream translation ───────────────────────────────────────────────────

fn data(value: &Value) -> String {
    format!("data: {value}\n\n")
}

fn chunk(delta: &Value, finish: Option<&str>) -> String {
    data(&json!({
        "id": "chatcmpl-xyz",
        "object": "chat.completion.chunk",
        "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
    }))
}

fn usage_chunk() -> String {
    data(&json!({
        "id": "chatcmpl-xyz",
        "choices": [],
        "usage": {
            "prompt_tokens": 321,
            "completion_tokens": 45,
            "prompt_tokens_details": { "cached_tokens": 100 },
            "completion_tokens_details": { "reasoning_tokens": 7 },
        },
    }))
}

fn translate_chunks(chunks: &[&[u8]]) -> Vec<LlmEvent> {
    let mut parser = SseParser::default();
    let mut translator = ChatCompletionsTranslator::default();
    let mut out = Vec::new();
    for chunk in chunks {
        for event in parser.push(chunk) {
            translator.translate(event, &mut out);
        }
    }
    for event in parser.finish() {
        translator.translate(event, &mut out);
    }
    out
}

fn completed_wire() -> String {
    [
        chunk(&json!({ "role": "assistant", "content": "" }), None),
        chunk(&json!({ "content": "Hello " }), None),
        chunk(&json!({ "content": "world." }), None),
        chunk(&json!({}), Some("stop")),
        usage_chunk(),
        "data: [DONE]\n\n".to_owned(),
    ]
    .concat()
}

fn expected_usage() -> LlmUsage {
    LlmUsage {
        input_tokens: 321,
        output_tokens: 45,
        cache_read_input_tokens: 100,
        cache_write_input_tokens: 0,
        reasoning_tokens: 7,
    }
}

#[test]
fn completed_stream_with_final_usage_chunk() {
    let expected = vec![
        LlmEvent::TextDelta("Hello ".to_owned()),
        LlmEvent::TextDelta("world.".to_owned()),
        LlmEvent::Completed {
            response_id: Some("chatcmpl-xyz".to_owned()),
            usage: Some(expected_usage()),
            incomplete_reason: None,
            output_text: "Hello world.".to_owned(),
        },
    ];
    let wire = completed_wire();
    assert_eq!(translate_chunks(&[wire.as_bytes()]), expected);
    let bytes = wire.as_bytes();
    let pieces: Vec<&[u8]> = bytes.chunks(7).collect();
    assert_eq!(translate_chunks(&pieces), expected, "chunk boundaries");
}

#[test]
fn finish_reason_length_completes_as_incomplete() {
    let wire = [
        chunk(&json!({ "content": "Truncated" }), None),
        chunk(&json!({}), Some("length")),
        "data: [DONE]\n\n".to_owned(),
    ]
    .concat();
    let events = translate_chunks(&[wire.as_bytes()]);
    let Some(LlmEvent::Completed {
        incomplete_reason,
        usage,
        ..
    }) = events.last()
    else {
        panic!("{events:?}");
    };
    assert_eq!(incomplete_reason.as_deref(), Some("max_tokens"));
    assert_eq!(*usage, None, "no usage chunk: usage unknown");
}

#[test]
fn end_of_body_after_finish_reason_completes() {
    let wire = [
        chunk(&json!({ "content": "Done" }), None),
        chunk(&json!({}), Some("stop")),
    ]
    .concat();
    let mut translator = ChatCompletionsTranslator::default();
    let mut parser = SseParser::default();
    let mut out = Vec::new();
    for event in parser.push(wire.as_bytes()) {
        translator.translate(event, &mut out);
    }
    assert!(matches!(
        translator.stream_ended(),
        LlmEvent::Completed { .. }
    ));
    let mut fresh = ChatCompletionsTranslator::default();
    assert_eq!(
        fresh.stream_ended(),
        LlmEvent::Failed(LlmFailure::provider_error(
            "provider stream ended unexpectedly"
        ))
    );
}

#[test]
fn tool_calls_become_function_call_tool_events() {
    let wire = [
        chunk(&json!({ "role": "assistant", "content": "" }), None),
        chunk(
            &json!({ "tool_calls": [{ "index": 0, "id": "call_cc0001", "type": "function",
                                      "function": { "name": "search_knowledge", "arguments": "" } }] }),
            None,
        ),
        chunk(
            &json!({ "tool_calls": [{ "index": 0, "function": { "arguments": "{\"query\"" } }] }),
            None,
        ),
        chunk(
            &json!({ "tool_calls": [{ "index": 0, "function": { "arguments": ":\"x\"}" } }] }),
            None,
        ),
        chunk(&json!({}), Some("tool_calls")),
        usage_chunk(),
        "data: [DONE]\n\n".to_owned(),
    ]
    .concat();
    let events = translate_chunks(&[wire.as_bytes()]);
    assert_eq!(
        events,
        vec![
            LlmEvent::ToolStarted {
                name: TOOL_FUNCTION_CALL.to_owned(),
                details: json!({ "index": 0, "call_id": "call_cc0001", "name": "search_knowledge" }),
            },
            LlmEvent::ToolCompleted {
                name: TOOL_FUNCTION_CALL.to_owned(),
                details: json!({ "call_id": "call_cc0001", "name": "search_knowledge",
                                 "arguments": "{\"query\":\"x\"}" }),
            },
            LlmEvent::FunctionCall(LlmFunctionCall {
                call_id: "call_cc0001".to_owned(),
                name: "search_knowledge".to_owned(),
                arguments: "{\"query\":\"x\"}".to_owned(),
            }),
            LlmEvent::Completed {
                response_id: Some("chatcmpl-xyz".to_owned()),
                usage: Some(expected_usage()),
                incomplete_reason: None,
                output_text: String::new(),
            },
        ]
    );
}

#[test]
fn error_chunk_is_a_sanitized_provider_error() {
    let wire = [
        chunk(&json!({ "content": "partial" }), None),
        data(
            &json!({ "error": { "message": RAW_MESSAGE, "type": "server_error",
                                 "code": "server_error", "param": null } }),
        ),
    ]
    .concat();
    let events = translate_chunks(&[wire.as_bytes()]);
    assert_eq!(events[0], LlmEvent::TextDelta("partial".to_owned()));
    let LlmEvent::Failed(failure) = &events[1] else {
        panic!("{events:?}");
    };
    assert_eq!(failure.kind, LlmFailureKind::ProviderError);
    assert_eq!(failure.provider_code.as_deref(), Some("server_error"));
    for leaked in [
        "chatcmpl-abc",
        "file-AbCdEf",
        "sk-AbCd",
        "https://internal-llm",
    ] {
        assert!(!failure.message_sanitized.contains(leaked), "{failure:?}");
    }
}

// ── Adapter over a fake gateway ──────────────────────────────────────────

fn target() -> ProviderTarget {
    ProviderTarget {
        provider_id: "cc".to_owned(),
        kind: ProviderKind::OpenaiChatCompletions,
        alias: "mock-cc".to_owned(),
        api_path: "/v1/chat/completions".to_owned(),
        tenant_id: Uuid::from_u128(7),
        tenant_override: false,
    }
}

fn adapter(gateway: &Arc<FakeGateway>) -> OpenAiChatCompletionsAdapter {
    OpenAiChatCompletionsAdapter::new(gateway.clone(), Arc::new(StaticSystemContext::default()))
}

#[tokio::test]
async fn stream_sends_through_oagw_and_translates() {
    let gateway = Arc::new(FakeGateway::default());
    gateway.push_proxy(Ok(sse_response(vec![completed_wire()])));
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
    assert!(matches!(events.last(), Some(LlmEvent::Completed { .. })));
    let sent = lock(&gateway.proxied)[0].clone();
    assert_eq!(sent.uri, "/mock-cc/v1/chat/completions");
    assert_eq!(sent.headers["accept"], "text/event-stream");
    assert_eq!(sent.body["model"], json!("gpt-4o-2024-08-06"));
}

#[tokio::test]
async fn http_429_is_rate_limited_before_streaming() {
    let gateway = Arc::new(FakeGateway::default());
    gateway.push_proxy(Ok(json_response(
        429,
        &json!({ "error": { "message": RAW_MESSAGE } }),
        &[("retry-after", "17")],
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
    assert_eq!(failure.retry_after_secs, Some(17));
    assert!(failure.message_sanitized.contains("17"));
}

#[tokio::test]
async fn complete_sends_a_non_streaming_request() {
    let gateway = Arc::new(FakeGateway::default());
    gateway.push_proxy(Ok(json_response(
        200,
        &json!({ "id": "chatcmpl-s", "choices": [{ "message": { "content": "summary" },
                                                    "finish_reason": "stop" }] }),
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
    assert_eq!(sent.headers["accept"], "application/json");
}
