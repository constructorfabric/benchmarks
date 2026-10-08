use mini_chat_sdk::{ApiParams, UsageTokens};
use serde_json::{Value, json};
use uuid::Uuid;

use super::*;
use crate::infra::llm::sanitize::provider_user_field;
use crate::infra::llm::types::{
    ContentPart, InputMessage, LlmTerminal, RawCitation, RequestMetadata, Role, StreamErrorCode,
    ToolSpec,
};

const TENANT: Uuid = Uuid::from_u128(0xa1);
const USER: Uuid = Uuid::from_u128(0xb2);
const CHAT: Uuid = Uuid::from_u128(0xc3);

fn no_params() -> ApiParams {
    ApiParams {
        temperature: None,
        top_p: None,
        frequency_penalty: None,
        presence_penalty: None,
        stop: Vec::new(),
        extra_body: None,
        reasoning_effort: None,
    }
}

fn req_with(tools: Vec<ToolSpec>) -> LlmRequest {
    let metadata = RequestMetadata::chat(TENANT, USER, CHAT, &tools);
    LlmRequest {
        model: "gpt-x".into(),
        instructions: "SYSTEM PROMPT".into(),
        input: vec![
            InputMessage::text(Role::User, "first question"),
            InputMessage::text(Role::Assistant, "first answer"),
            InputMessage::text(Role::User, "current question"),
        ],
        max_output_tokens: 4096,
        tools,
        max_tool_calls: 2,
        api_params: no_params(),
        user: provider_user_field(TENANT, USER),
        metadata,
        stream: true,
    }
}

fn body(req: &LlmRequest) -> Value {
    OpenAiResponsesAdapter.build_body(req)
}

fn ev(name: Option<&str>, data: &Value) -> SseEvent {
    SseEvent {
        event: name.map(str::to_owned),
        data: data.to_string(),
    }
}

fn typed(kind: &str, mut data: Value) -> SseEvent {
    data["type"] = json!(kind);
    ev(Some(kind), &data)
}

fn parse_all(events: &[SseEvent]) -> Vec<LlmEvent> {
    let mut state = ParseState::default();
    events
        .iter()
        .flat_map(|e| OpenAiResponsesAdapter.parse_event(e, &mut state))
        .collect()
}

// ---------------------------------------------------------------------------
// Request body
// ---------------------------------------------------------------------------

#[test]
fn body_minimal_chat() {
    let b = body(&req_with(Vec::new()));
    assert_eq!(b["model"], "gpt-x");
    assert_eq!(b["stream"], true);
    assert_eq!(b["store"], false);
    assert_eq!(b["max_output_tokens"], 4096);
    assert_eq!(b["max_tool_calls"], 2);
    assert!(
        b.get("tools")
            .is_none_or(|t| t.as_array().is_some_and(Vec::is_empty)),
        "no tools expected: {b}"
    );
    assert!(b.get("include").is_none());
    let user = b["user"].as_str().unwrap();
    assert_eq!(user.len(), 64);
    assert_eq!(user, provider_user_field(TENANT, USER));
    assert_eq!(b["metadata"]["request_type"], "chat");
    assert_eq!(b["metadata"]["feature"], "none");
    assert_eq!(b["instructions"], "SYSTEM PROMPT");
    let input = b["input"].as_array().unwrap();
    assert_eq!(input.len(), 3);
    assert_eq!(
        input[0],
        json!({"role": "user", "content": "first question"})
    );
    assert_eq!(
        input[1],
        json!({"role": "assistant", "content": "first answer"})
    );
    let last = input.last().unwrap();
    assert_eq!(last["role"], "user");
    assert_eq!(last["content"], "current question");
}

#[test]
fn body_metadata_carries_ids() {
    let b = body(&req_with(Vec::new()));
    assert_eq!(
        b["metadata"],
        json!({
            "tenant_id": TENANT.to_string(),
            "user_id": USER.to_string(),
            "chat_id": CHAT.to_string(),
            "request_type": "chat",
            "feature": "none",
        })
    );
}

#[test]
fn body_with_all_tools() {
    let b = body(&req_with(vec![
        ToolSpec::FileSearch {
            vector_store_ids: vec!["vs_1".into()],
            max_num_results: 5,
        },
        ToolSpec::WebSearch {
            search_context_size: "low".into(),
        },
        ToolSpec::CodeInterpreter {
            file_ids: vec!["file-x".into()],
        },
    ]));
    assert_eq!(
        b["tools"],
        json!([
            {"type": "file_search", "vector_store_ids": ["vs_1"], "max_num_results": 5},
            {"type": "web_search", "search_context_size": "low"},
            {"type": "code_interpreter", "container": {"type": "auto", "file_ids": ["file-x"]}},
        ])
    );
    assert_eq!(b["include"], json!(["code_interpreter_call.outputs"]));
    assert_eq!(
        b["metadata"]["feature"],
        "file_search+web_search+code_interpreter"
    );
}

#[test]
fn body_function_tool_does_not_count_as_feature() {
    let params = json!({"type": "object", "properties": {"query": {"type": "string"}}});
    let b = body(&req_with(vec![ToolSpec::Function {
        name: "search_knowledge".into(),
        description: "Search the knowledge base".into(),
        parameters: params.clone(),
    }]));
    assert_eq!(
        b["tools"],
        json!([{
            "type": "function",
            "name": "search_knowledge",
            "description": "Search the knowledge base",
            "parameters": params,
        }])
    );
    assert!(b.get("include").is_none());
    assert_eq!(b["metadata"]["feature"], "none");
}

#[test]
fn body_images() {
    let mut req = req_with(Vec::new());
    req.input.last_mut().unwrap().content = vec![
        ContentPart::Text("what is this?".into()),
        ContentPart::Image {
            file_id: "file-img".into(),
        },
    ];
    let b = body(&req);
    assert_eq!(
        b["input"].as_array().unwrap().last().unwrap(),
        &json!({
            "role": "user",
            "content": [
                {"type": "input_text", "text": "what is this?"},
                {"type": "input_image", "file_id": "file-img"},
            ],
        })
    );
    // History stays string content.
    assert_eq!(b["input"][0]["content"], "first question");
}

#[test]
fn body_replays_function_call_and_output_items() {
    let mut req = req_with(Vec::new());
    req.input.push(InputMessage {
        role: Role::Assistant,
        content: vec![ContentPart::FunctionCall {
            call_id: "call_1".into(),
            name: "search_knowledge".into(),
            arguments: r#"{"query":"q"}"#.into(),
        }],
    });
    req.input.push(InputMessage {
        role: Role::User,
        content: vec![ContentPart::FunctionOutput {
            call_id: "call_1".into(),
            output: "RESULTS".into(),
        }],
    });
    let b = body(&req);
    let input = b["input"].as_array().unwrap();
    assert_eq!(input.len(), 5);
    assert_eq!(
        input[3],
        json!({"type": "function_call", "call_id": "call_1", "name": "search_knowledge", "arguments": r#"{"query":"q"}"#})
    );
    assert_eq!(
        input[4],
        json!({"type": "function_call_output", "call_id": "call_1", "output": "RESULTS"})
    );
}

#[test]
fn api_params_sent_only_when_set() {
    let b = body(&req_with(Vec::new()));
    for key in [
        "temperature",
        "top_p",
        "frequency_penalty",
        "presence_penalty",
        "stop",
        "reasoning",
    ] {
        assert!(b.get(key).is_none(), "{key} must be absent: {b}");
    }

    let mut req = req_with(Vec::new());
    req.api_params = ApiParams {
        temperature: Some(0.7),
        top_p: Some(1.0),
        frequency_penalty: Some(0.0),
        presence_penalty: Some(0.5),
        stop: vec!["END".into()],
        extra_body: None,
        reasoning_effort: Some("low".into()),
    };
    let b = body(&req);
    assert_eq!(b["temperature"], 0.7);
    assert_eq!(b["top_p"], 1.0);
    assert_eq!(b["frequency_penalty"], 0.0);
    assert_eq!(b["presence_penalty"], 0.5);
    assert_eq!(b["stop"], json!(["END"]));
    assert_eq!(b["reasoning"], json!({"effort": "low"}));
}

#[test]
fn extra_body_merged_but_controlled_keys_ignored() {
    let mut req = req_with(Vec::new());
    let extra = json!({
        "foo": 1,
        "model": "x",
        "stream": false,
        "user": "someone",
        "metadata": {},
        "tools": [],
        "store": true,
        "service_tier": "flex",
    });
    req.api_params.extra_body = Some(extra.as_object().unwrap().clone());
    let b = body(&req);
    assert_eq!(b["foo"], 1);
    assert_eq!(b["service_tier"], "flex");
    assert_eq!(b["model"], "gpt-x");
    assert_eq!(b["stream"], true);
    assert_eq!(b["store"], false);
    assert_eq!(b["user"], provider_user_field(TENANT, USER));
    assert_eq!(b["metadata"]["request_type"], "chat");
    assert!(b.get("tools").is_none());
}

#[test]
fn summary_body_is_non_streaming_with_summary_metadata() {
    let system_user = Uuid::parse_str("11111111-6a88-4768-9dfc-6bcd5187d9ed").unwrap();
    let mut req = req_with(Vec::new());
    req.stream = false;
    req.metadata = RequestMetadata::summary(TENANT, system_user, CHAT);
    req.user = provider_user_field(TENANT, system_user);
    let b = body(&req);
    assert_eq!(b["stream"], false);
    assert_eq!(b["metadata"]["request_type"], "summary");
    assert_eq!(b["metadata"]["feature"], "none");
    assert_eq!(b["metadata"]["user_id"], system_user.to_string());
}

// ---------------------------------------------------------------------------
// Stream parsing
// ---------------------------------------------------------------------------

#[test]
fn parse_stream() {
    let logs_a = "a".repeat(5000);
    let logs_b = "b".repeat(4000);
    let events = vec![
        typed(
            "response.output_text.delta",
            json!({"output_index": 0, "content_index": 0, "delta": "Hello"}),
        ),
        // No `event:` line: the name comes from `type`.
        ev(
            None,
            &json!({"type": "response.output_text.delta", "output_index": 0, "content_index": 0, "delta": " world"}),
        ),
        typed(
            "response.file_search_call.searching",
            json!({"item_id": "fs_1"}),
        ),
        typed(
            "response.file_search_call.completed",
            json!({"item_id": "fs_1", "results": [{}, {}]}),
        ),
        typed(
            "response.web_search_call.searching",
            json!({"item_id": "ws_1"}),
        ),
        typed(
            "response.web_search_call.completed",
            json!({"item_id": "ws_1"}),
        ),
        typed(
            "response.code_interpreter_call.in_progress",
            json!({"item_id": "ci_1"}),
        ),
        typed(
            "response.code_interpreter_call.interpreting",
            json!({"item_id": "ci_1"}),
        ),
        typed(
            "response.code_interpreter_call.completed",
            json!({"item_id": "ci_1"}),
        ),
        typed(
            "response.output_item.done",
            json!({"output_index": 1, "item": {
                "type": "code_interpreter_call",
                "id": "ci_1",
                "outputs": [
                    {"type": "logs", "logs": logs_a},
                    {"type": "image", "url": "https://x"},
                    {"type": "logs", "logs": logs_b},
                ],
            }}),
        ),
        typed(
            "response.output_text.annotation.added",
            json!({"output_index": 0, "content_index": 0, "annotation_index": 0, "annotation": {
                "type": "url_citation", "url": "https://example.com/a", "title": "Example",
                "start_index": 0, "end_index": 5,
            }}),
        ),
        typed(
            "response.output_text.annotation.added",
            json!({"output_index": 0, "content_index": 0, "annotation_index": 1, "annotation": {
                "type": "file_citation", "file_id": "file-abc", "filename": "doc.pdf", "index": 11,
            }}),
        ),
        // Annotations already streamed: the message item does not repeat them.
        typed(
            "response.output_item.done",
            json!({"output_index": 0, "item": {"type": "message", "content": [{
                "type": "output_text", "text": "Hello world",
                "annotations": [{"type": "url_citation", "url": "https://example.com/a",
                                 "title": "Example", "start_index": 0, "end_index": 5}],
            }]}}),
        ),
        typed("response.output_text.done", json!({"text": "Hello world"})),
        typed(
            "response.completed",
            json!({"response": {
                "id": "resp_1",
                "status": "completed",
                "usage": {
                    "input_tokens": 100,
                    "output_tokens": 20,
                    "input_tokens_details": {"cached_tokens": 30},
                    "output_tokens_details": {"reasoning_tokens": 5},
                },
            }}),
        ),
    ];

    let joined = format!("{logs_a}\n{logs_b}");
    let expected_output = format!("{}...[truncated]", &joined[..8192]);

    assert_eq!(
        parse_all(&events),
        vec![
            LlmEvent::TextDelta("Hello".into()),
            LlmEvent::TextDelta(" world".into()),
            LlmEvent::ToolStart {
                name: "file_search".into(),
                details: json!({}),
            },
            LlmEvent::ToolDone {
                name: "file_search".into(),
                details: json!({"files_searched": 2}),
            },
            LlmEvent::ToolStart {
                name: "web_search".into(),
                details: json!({}),
            },
            LlmEvent::ToolDone {
                name: "web_search".into(),
                details: json!({}),
            },
            LlmEvent::ToolStart {
                name: "code_interpreter".into(),
                details: json!({}),
            },
            LlmEvent::ToolDone {
                name: "code_interpreter".into(),
                details: json!({"output": expected_output}),
            },
            LlmEvent::Citation(RawCitation::Url {
                url: "https://example.com/a".into(),
                title: "Example".into(),
                start: Some(0),
                end: Some(5),
                snippet: "Hello".into(),
            }),
            LlmEvent::Citation(RawCitation::File {
                provider_file_id: "file-abc".into(),
                filename: "doc.pdf".into(),
            }),
            LlmEvent::Completed(LlmTerminal {
                usage: Some(UsageTokens {
                    input_tokens: 100,
                    output_tokens: 20,
                    cache_read_input_tokens: 30,
                    cache_write_input_tokens: 0,
                    reasoning_tokens: 5,
                }),
                response_id: Some("resp_1".into()),
            }),
        ]
    );
}

#[test]
fn file_search_completed_without_results_counts_zero() {
    let out = parse_all(&[typed(
        "response.file_search_call.completed",
        json!({"item_id": "fs_1"}),
    )]);
    assert_eq!(
        out,
        vec![LlmEvent::ToolDone {
            name: "file_search".into(),
            details: json!({"files_searched": 0}),
        }]
    );
}

#[test]
fn annotations_from_message_item_when_not_streamed() {
    let out = parse_all(&[typed(
        "response.output_item.done",
        json!({"output_index": 0, "item": {"type": "message", "content": [{
            "type": "output_text", "text": "Hello world",
            "annotations": [
                {"type": "url_citation", "url": "https://e.x/1", "title": "One",
                 "start_index": 6, "end_index": 11},
                {"type": "url_citation", "url": "https://e.x/2", "title": "Two",
                 "start_index": 50, "end_index": 60},
                {"type": "file_citation", "file_id": "file-q", "filename": "q.txt"},
            ],
        }]}}),
    )]);
    assert_eq!(
        out,
        vec![
            LlmEvent::Citation(RawCitation::Url {
                url: "https://e.x/1".into(),
                title: "One".into(),
                start: Some(6),
                end: Some(11),
                snippet: "world".into(),
            }),
            // Range outside the text: empty snippet.
            LlmEvent::Citation(RawCitation::Url {
                url: "https://e.x/2".into(),
                title: "Two".into(),
                start: Some(50),
                end: Some(60),
                snippet: String::new(),
            }),
            LlmEvent::Citation(RawCitation::File {
                provider_file_id: "file-q".into(),
                filename: "q.txt".into(),
            }),
        ]
    );
}

#[test]
fn parse_ignores_reasoning_deltas_and_emits_function_call() {
    // D: only the vLLM adapter emits `reasoning` deltas (from `<think>`
    // text); OpenAI reasoning events have no client event.
    let out = parse_all(&[
        typed(
            "response.reasoning_summary_text.delta",
            json!({"delta": "thinking"}),
        ),
        typed(
            "response.reasoning_text.delta",
            json!({"delta": "more thinking"}),
        ),
        typed(
            "response.output_item.done",
            json!({"output_index": 2, "item": {
                "type": "function_call", "call_id": "call_1", "name": "search_knowledge",
                "arguments": "{\"query\":\"x\"}",
            }}),
        ),
        typed("response.created", json!({"response": {"id": "resp_9"}})),
    ]);
    assert_eq!(
        out,
        vec![
            LlmEvent::FunctionCall {
                call_id: "call_1".into(),
                name: "search_knowledge".into(),
                arguments: "{\"query\":\"x\"}".into(),
            },
        ]
    );
}

#[test]
fn parse_incomplete() {
    let out = parse_all(&[typed(
        "response.incomplete",
        json!({"response": {
            "id": "resp_2",
            "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "usage": {"input_tokens": 10, "output_tokens": 4096},
        }}),
    )]);
    assert_eq!(
        out,
        vec![LlmEvent::Incomplete {
            terminal: LlmTerminal {
                usage: Some(UsageTokens {
                    input_tokens: 10,
                    output_tokens: 4096,
                    ..UsageTokens::default()
                }),
                response_id: Some("resp_2".into()),
            },
            reason: "max_output_tokens".into(),
        }]
    );
}

#[test]
fn parse_failed_uses_response_error_and_keeps_usage() {
    let out = parse_all(&[typed(
        "response.failed",
        json!({
            "response": {
                "id": "resp_3",
                "status": "failed",
                "error": {"code": "server_error", "message": "Failed reading file-abc123def456ghi"},
                "usage": {"input_tokens": 12, "output_tokens": 3},
            },
            "error": {"message": "top-level message is only a fallback"},
        }),
    )]);
    assert_eq!(
        out,
        vec![LlmEvent::Failed(ProviderFailure {
            code: StreamErrorCode::ProviderError,
            message: "Failed reading [provider_id]".into(),
            usage: Some(UsageTokens {
                input_tokens: 12,
                output_tokens: 3,
                ..UsageTokens::default()
            }),
        })]
    );

    // Fallback: top-level `error`.
    let out = parse_all(&[typed(
        "response.failed",
        json!({"response": {"id": "resp_4"}, "error": {"message": "fallback text"}}),
    )]);
    assert_eq!(
        out,
        vec![LlmEvent::Failed(ProviderFailure::new(
            StreamErrorCode::ProviderError,
            "fallback text"
        ))]
    );
}

#[test]
fn parse_error_event_flat() {
    let out = parse_all(&[
        ev(
            Some("error"),
            &json!({"type": "error", "code": "rate_limit_exceeded", "message": "Slow down sk-ABCDEFGHIJKL", "param": null}),
        ),
        SseEvent {
            event: Some("error".into()),
            data: "upstream exploded for resp_abc".into(),
        },
        ev(
            Some("error"),
            &json!({"type": "error", "error": {"code": "x", "message": "nested see https://a.b/c"}}),
        ),
    ]);
    assert_eq!(
        out,
        vec![
            LlmEvent::Failed(ProviderFailure::new(
                StreamErrorCode::ProviderError,
                "Slow down [credential]"
            )),
            LlmEvent::Failed(ProviderFailure::new(
                StreamErrorCode::ProviderError,
                "upstream exploded for [provider_id]"
            )),
            LlmEvent::Failed(ProviderFailure::new(
                StreamErrorCode::ProviderError,
                "nested see [url]"
            )),
        ]
    );
}

// ---------------------------------------------------------------------------
// Non-streaming
// ---------------------------------------------------------------------------

#[test]
fn completion_extracts_text_usage_and_id() {
    let body = json!({
        "id": "resp_s",
        "status": "completed",
        "output": [
            {"type": "reasoning", "summary": []},
            {"type": "message", "role": "assistant", "content": [
                {"type": "output_text", "text": "<summary>A</summary>", "annotations": []},
                {"type": "output_text", "text": " more"},
            ]},
        ],
        "usage": {"input_tokens": 7, "output_tokens": 3},
    });
    assert_eq!(
        OpenAiResponsesAdapter.parse_completion(&body),
        Ok(LlmCompletion {
            text: "<summary>A</summary> more".into(),
            usage: Some(UsageTokens {
                input_tokens: 7,
                output_tokens: 3,
                ..UsageTokens::default()
            }),
            response_id: Some("resp_s".into()),
        })
    );
}

#[test]
fn completion_failed_status_is_sanitized_failure() {
    let body = json!({
        "id": "resp_f",
        "status": "failed",
        "error": {"code": "server_error", "message": "boom on vs_abcdefghijklmn"},
    });
    assert_eq!(
        OpenAiResponsesAdapter.parse_completion(&body),
        Err(ProviderFailure::new(
            StreamErrorCode::ProviderError,
            "boom on [provider_id]"
        ))
    );
}
