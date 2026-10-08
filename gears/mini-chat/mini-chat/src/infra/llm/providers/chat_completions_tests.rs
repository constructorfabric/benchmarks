#![allow(clippy::unwrap_used, clippy::expect_used)]

use futures::StreamExt;
use mini_chat_sdk::{ModelApiParams, UsageTokens};
use oagw_sdk::api::ErrorSource;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::config::ProviderKind;
use crate::infra::llm::fake_gw::{FakeGw, Reply, client, http_error, json_ok, sse_data};
use crate::infra::llm::types::{
    ContentPart, InputItem, LlmClient, LlmEvent, LlmRequest, ProviderError, RequestMetadata,
    ResolvedProvider, ToolSpec, provider_user,
};

const TENANT: &str = "6f1d7a52-0f6e-4c37-9a3c-0d7a8f3c2b11";
const USER: &str = "0b9e4c1d-2f3a-4b5c-8d6e-7f8091a2b3c4";

fn provider() -> ResolvedProvider {
    ResolvedProvider {
        provider_id: "chat".to_owned(),
        kind: ProviderKind::OpenaiChatCompletions,
        alias: "llm.example.com".to_owned(),
        api_path: "/v1/chat/completions".to_owned(),
        storage: None,
    }
}

fn request() -> LlmRequest {
    LlmRequest {
        model: "gpt-4.1".to_owned(),
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

fn chunk(delta: &Value, finish_reason: Option<&str>) -> Value {
    json!({
        "id": "chatcmpl-1", "object": "chat.completion.chunk", "model": "gpt-4.1",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}]
    })
}

fn usage_chunk() -> Value {
    json!({
        "id": "chatcmpl-1", "object": "chat.completion.chunk", "choices": [],
        "usage": {
            "prompt_tokens": 120, "completion_tokens": 40, "total_tokens": 160,
            "prompt_tokens_details": {"cached_tokens": 100},
            "completion_tokens_details": {"reasoning_tokens": 8}
        }
    })
}

fn usage() -> UsageTokens {
    UsageTokens {
        input_tokens: 120,
        output_tokens: 40,
        cache_read_input_tokens: 100,
        cache_write_input_tokens: 0,
        reasoning_tokens: 8,
    }
}

// ── Request ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn builds_chat_completions_request() {
    let gw = FakeGw::with(sse_data(&[usage_chunk()], true));
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
        call_id: "call_1".to_owned(),
        name: "search_knowledge".to_owned(),
        arguments: r#"{"query":"a"}"#.to_owned(),
    });
    req.input.push(InputItem::FunctionCall {
        call_id: "call_2".to_owned(),
        name: "search_knowledge".to_owned(),
        arguments: r#"{"query":"b"}"#.to_owned(),
    });
    req.input.push(InputItem::FunctionCallOutput {
        call_id: "call_1".to_owned(),
        output: "A".to_owned(),
    });
    req.input.push(InputItem::FunctionCallOutput {
        call_id: "call_2".to_owned(),
        output: "B".to_owned(),
    });
    req.api_params.temperature = Some(0.3);
    req.api_params.stop = vec!["END".to_owned()];
    req.api_params.reasoning_effort = Some("low".to_owned());
    req.api_params.extra_body = Some(
        json!({"seed": 7, "messages": [], "stream_options": {}})
            .as_object()
            .unwrap()
            .clone(),
    );
    let stream = client
        .stream(&provider(), req, CancellationToken::new())
        .await
        .unwrap();
    let _: Vec<_> = stream.collect().await;

    let cap = gw.last();
    assert_eq!(cap.uri, "/llm.example.com/v1/chat/completions");
    let body = cap.body;
    assert_eq!(body["model"], "gpt-4.1");
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"], json!({"include_usage": true}));
    assert_eq!(body["max_completion_tokens"], 1024);
    assert_eq!(
        body["user"],
        "6f1d7a520f6e4c379a3c0d7a8f3c2b110b9e4c1d2f3a4b5c8d6e7f8091a2b3c4"
    );
    assert!(body.get("metadata").is_none(), "no metadata");
    assert!(body.get("max_tool_calls").is_none());
    assert!(body.get("instructions").is_none());
    assert_eq!(
        body["messages"],
        json!([
            {"role": "system", "content": "be helpful"},
            {"role": "user", "content": "what is this?"},
            {"role": "assistant", "content": "a cat"},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "call_1", "type": "function", "function": {"name": "search_knowledge", "arguments": "{\"query\":\"a\"}"}},
                {"id": "call_2", "type": "function", "function": {"name": "search_knowledge", "arguments": "{\"query\":\"b\"}"}}
            ]},
            {"role": "tool", "tool_call_id": "call_1", "content": "A"},
            {"role": "tool", "tool_call_id": "call_2", "content": "B"}
        ])
    );
    // Built-in tools dropped, function tools kept.
    assert_eq!(
        body["tools"],
        json!([{"type": "function", "function": {
            "name": "search_knowledge", "description": "kb", "parameters": {"type": "object"}
        }}])
    );
    assert_eq!(body["temperature"], 0.3);
    assert_eq!(body["stop"], json!(["END"]));
    assert_eq!(body["reasoning_effort"], "low");
    assert_eq!(body["seed"], 7);
}

#[tokio::test]
async fn only_builtin_tools_means_no_tools_key() {
    let gw = FakeGw::with(sse_data(&[usage_chunk()], true));
    let (client, _) = client(&gw);
    let mut req = request();
    req.tools = vec![ToolSpec::WebSearch {
        search_context_size: "low".to_owned(),
    }];
    let stream = client
        .stream(&provider(), req, CancellationToken::new())
        .await
        .unwrap();
    let _: Vec<_> = stream.collect().await;
    let body = gw.last().body;
    assert!(body.get("tools").is_none());
    assert!(body.get("stop").is_none());
    assert!(body.get("temperature").is_none());
}

// ── Streaming ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn streams_text_and_normalized_usage() {
    let events = run(sse_data(
        &[
            chunk(&json!({"role": "assistant", "content": ""}), None),
            chunk(&json!({"content": "Hel"}), None),
            chunk(&json!({"content": "lo"}), None),
            chunk(&json!({}), Some("stop")),
            usage_chunk(),
        ],
        true,
    ))
    .await;
    assert_eq!(
        events,
        vec![
            LlmEvent::TextDelta("Hel".to_owned()),
            LlmEvent::TextDelta("lo".to_owned()),
            LlmEvent::Completed {
                usage: Some(usage()),
                response_id: Some("chatcmpl-1".to_owned()),
                incomplete_reason: None,
            },
        ]
    );
}

#[tokio::test]
async fn tool_calls_emit_function_call_events() {
    let events = run(sse_data(
        &[
            chunk(
                &json!({"tool_calls": [{"index": 0, "id": "call_7", "type": "function",
                    "function": {"name": "search_knowledge", "arguments": ""}}]}),
                None,
            ),
            chunk(
                &json!({"tool_calls": [{"index": 0, "function": {"arguments": "{\"query\":"}}]}),
                None,
            ),
            chunk(
                &json!({"tool_calls": [{"index": 0, "function": {"arguments": "\"x\"}"}}]}),
                None,
            ),
            chunk(&json!({}), Some("tool_calls")),
            usage_chunk(),
        ],
        true,
    ))
    .await;
    assert_eq!(
        events,
        vec![
            LlmEvent::ToolStart {
                name: "function_call".to_owned(),
                details: json!({"index": 0, "call_id": "call_7", "name": "search_knowledge"}),
            },
            LlmEvent::ToolDone {
                name: "function_call".to_owned(),
                details: json!({"call_id": "call_7", "name": "search_knowledge", "arguments": "{\"query\":\"x\"}"}),
            },
            LlmEvent::FunctionCall {
                call_id: "call_7".to_owned(),
                name: "search_knowledge".to_owned(),
                arguments: "{\"query\":\"x\"}".to_owned(),
            },
            LlmEvent::Completed {
                usage: Some(usage()),
                response_id: Some("chatcmpl-1".to_owned()),
                incomplete_reason: None,
            },
        ]
    );
}

#[tokio::test]
async fn finish_reason_length_is_incomplete_max_tokens() {
    let events = run(sse_data(
        &[
            chunk(&json!({"content": "trunc"}), None),
            chunk(&json!({}), Some("length")),
            usage_chunk(),
        ],
        true,
    ))
    .await;
    assert_eq!(
        events.last().unwrap(),
        &LlmEvent::Completed {
            usage: Some(usage()),
            response_id: Some("chatcmpl-1".to_owned()),
            incomplete_reason: Some("max_tokens".to_owned()),
        }
    );
}

#[tokio::test]
async fn body_end_after_finish_reason_without_done_completes() {
    let events = run(sse_data(
        &[
            chunk(&json!({"content": "a"}), None),
            chunk(&json!({}), Some("stop")),
        ],
        false,
    ))
    .await;
    assert_eq!(
        events.last().unwrap(),
        &LlmEvent::Completed {
            usage: None,
            response_id: Some("chatcmpl-1".to_owned()),
            incomplete_reason: None,
        }
    );
}

#[tokio::test]
async fn empty_response_id_is_none_and_later_id_is_kept() {
    let with_id = |id: &str, delta: &Value, fin: Option<&str>| {
        let mut c = chunk(delta, fin);
        c["id"] = json!(id);
        c
    };
    let only_empty = run(sse_data(
        &[
            with_id("", &json!({"content": "a"}), None),
            with_id("", &json!({}), Some("stop")),
        ],
        true,
    ))
    .await;
    assert_eq!(
        only_empty.last().unwrap(),
        &LlmEvent::Completed {
            usage: None,
            response_id: None,
            incomplete_reason: None,
        }
    );
    let late = run(sse_data(
        &[
            with_id("", &json!({"content": "a"}), None),
            with_id("chatcmpl-9", &json!({}), Some("stop")),
        ],
        true,
    ))
    .await;
    assert_eq!(
        late.last().unwrap(),
        &LlmEvent::Completed {
            usage: None,
            response_id: Some("chatcmpl-9".to_owned()),
            incomplete_reason: None,
        }
    );
}

// ── Errors ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn in_stream_error_is_failed_and_sanitized() {
    let events = run(sse_data(
        &[json!({"error": {"message": "boom at resp_abcdef", "code": "server_error"}})],
        false,
    ))
    .await;
    assert_eq!(
        events,
        vec![LlmEvent::Failed {
            error: ProviderError::provider("boom at [provider_id]"),
            usage: None,
        }]
    );
}

#[tokio::test]
async fn http_error_maps_through_shared_status_mapping() {
    let gw = FakeGw::with(http_error(
        400,
        ErrorSource::Upstream,
        vec![],
        &json!({"error": {"code": "context_length_exceeded", "message": "too long"}}),
    ));
    let (client, _) = client(&gw);
    let Err(err) = client
        .stream(&provider(), request(), CancellationToken::new())
        .await
    else {
        panic!("expected an error");
    };
    assert_eq!(err.code, "provider_error");
    assert!(err.context_length_exceeded);
}

// ── Non-streaming ────────────────────────────────────────────────────────────

#[tokio::test]
async fn complete_returns_message_text_and_usage() {
    let gw = FakeGw::with(json_ok(&json!({
        "id": "chatcmpl-2",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "Summary"}, "finish_reason": "stop"}],
        "usage": usage_chunk()["usage"]
    })));
    let (client, _) = client(&gw);
    let out = client.complete(&provider(), request()).await.unwrap();
    assert_eq!(out.text, "Summary");
    assert_eq!(out.usage, Some(usage()));
    let body = gw.last().body;
    assert_eq!(body["stream"], false);
    assert!(body.get("stream_options").is_none());
}
