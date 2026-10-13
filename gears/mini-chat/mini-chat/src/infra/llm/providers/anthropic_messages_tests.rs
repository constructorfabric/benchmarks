use mini_chat_sdk::UsageTokens;
use serde_json::{Value, json};

use super::*;
use crate::infra::llm::providers::test_util::{
    TENANT, USER, all_tools, ev, knowledge_tool, parse_all, req_with,
};
use crate::infra::llm::sanitize::provider_user_field;
use crate::infra::llm::types::{
    ContentPart, InputMessage, LlmTerminal, Role, StreamErrorCode, ToolSpec,
};

/// Anthropic SSE event: `event:` line plus `type` in the data.
fn a(kind: &str, mut data: Value) -> SseEvent {
    data["type"] = json!(kind);
    ev(Some(kind), &data)
}

fn parse(events: &[SseEvent]) -> Vec<LlmEvent> {
    parse_all(&AnthropicMessagesAdapter, events)
}

fn message_start() -> SseEvent {
    a(
        "message_start",
        json!({"message": {
            "id": "msg_01abc", "type": "message", "role": "assistant", "content": [],
            "usage": {"input_tokens": 100, "cache_creation_input_tokens": 20,
                      "cache_read_input_tokens": 30, "output_tokens": 1},
        }}),
    )
}

fn message_end(stop_reason: &str, output_tokens: i64) -> [SseEvent; 2] {
    [
        a(
            "message_delta",
            json!({"delta": {"stop_reason": stop_reason, "stop_sequence": null},
                   "usage": {"output_tokens": output_tokens}}),
        ),
        a("message_stop", json!({})),
    ]
}

#[test]
fn body_system_messages_max_tokens_metadata_user_id() {
    let mut req = req_with(Vec::new());
    req.api_params.temperature = Some(0.5);
    req.api_params.frequency_penalty = Some(0.1);
    req.api_params.stop = vec!["STOP".into()];
    req.api_params.extra_body = Some(serde_json::Map::from_iter([(
        "custom".to_owned(),
        json!(1),
    )]));
    let b = AnthropicMessagesAdapter.build_body(&req);
    assert_eq!(b["model"], "model-x");
    assert_eq!(b["system"], "SYSTEM PROMPT");
    assert_eq!(b["max_tokens"], 4096);
    assert_eq!(b["stream"], true);
    assert_eq!(
        b["metadata"],
        json!({"user_id": provider_user_field(TENANT, USER)})
    );
    assert_eq!(
        b["messages"],
        json!([
            {"role": "user", "content": "first question"},
            {"role": "assistant", "content": "first answer"},
            {"role": "user", "content": "current question"},
        ])
    );
    assert_eq!(b["temperature"], 0.5);
    assert_eq!(b["stop_sequences"], json!(["STOP"]));
    for absent in [
        "user",
        "input",
        "instructions",
        "max_output_tokens",
        "max_tool_calls",
        "frequency_penalty",
        "custom",
        "tools",
    ] {
        assert!(b.get(absent).is_none(), "{absent} must not be sent: {b}");
    }
}

#[test]
fn web_search_and_code_execution_mapped_file_search_dropped() {
    let b = AnthropicMessagesAdapter.build_body(&req_with(all_tools()));
    assert_eq!(
        b["tools"],
        json!([
            {"type": "web_search_20250305", "name": "web_search"},
            {"type": "code_execution_20250825", "name": "code_execution"},
            {"name": "search_knowledge", "description": "Search the knowledge base",
             "input_schema": {"type": "object", "properties": {"query": {"type": "string"}}}},
        ])
    );
    let headers = AnthropicMessagesAdapter.extra_headers(&req_with(all_tools()));
    assert!(headers.contains(&("anthropic-version", "2023-06-01".to_owned())));
    assert!(headers.contains(&("anthropic-beta", "code-execution-2025-08-25".to_owned())));

    let only_file_search = vec![ToolSpec::FileSearch {
        vector_store_ids: vec!["vs_abcdefghijklmnop".into()],
        max_num_results: 5,
    }];
    let req = req_with(only_file_search);
    assert!(
        AnthropicMessagesAdapter
            .build_body(&req)
            .get("tools")
            .is_none()
    );
    assert_eq!(
        AnthropicMessagesAdapter.extra_headers(&req),
        vec![("anthropic-version", "2023-06-01".to_owned())]
    );
}

#[test]
fn body_images_and_tool_replay() {
    let mut req = req_with(vec![knowledge_tool()]);
    req.input[2].content.push(ContentPart::Image {
        file_id: "file_011abc".into(),
    });
    req.input.push(InputMessage {
        role: Role::Assistant,
        content: vec![ContentPart::FunctionCall {
            call_id: "toolu_1".into(),
            name: "search_knowledge".into(),
            arguments: r#"{"query":"vacation"}"#.into(),
        }],
    });
    req.input.push(InputMessage {
        role: Role::User,
        content: vec![ContentPart::FunctionOutput {
            call_id: "toolu_1".into(),
            output: "RESULTS".into(),
        }],
    });
    let b = AnthropicMessagesAdapter.build_body(&req);
    let messages = b["messages"].as_array().unwrap();
    assert_eq!(
        messages[2],
        json!({"role": "user", "content": [
            {"type": "text", "text": "current question"},
            {"type": "image", "source": {"type": "file", "file_id": "file_011abc"}},
        ]})
    );
    assert_eq!(
        messages[3],
        json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": "toolu_1", "name": "search_knowledge", "input": {"query": "vacation"}},
        ]})
    );
    assert_eq!(
        messages[4],
        json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_1", "content": "RESULTS"},
        ]})
    );
}

#[test]
fn parses_message_events_usage_and_max_tokens_incomplete() {
    let mut events = vec![
        message_start(),
        a(
            "content_block_start",
            json!({"index": 0, "content_block": {"type": "text", "text": ""}}),
        ),
        a("ping", json!({})),
        a(
            "content_block_delta",
            json!({"index": 0, "delta": {"type": "text_delta", "text": "Hel"}}),
        ),
        a(
            "content_block_delta",
            json!({"index": 0, "delta": {"type": "text_delta", "text": "lo"}}),
        ),
        a("content_block_stop", json!({"index": 0})),
    ];
    events.extend(message_end("max_tokens", 50));
    let usage = UsageTokens {
        input_tokens: 150,
        output_tokens: 50,
        cache_read_input_tokens: 30,
        cache_write_input_tokens: 20,
        reasoning_tokens: 0,
    };
    assert_eq!(
        parse(&events),
        vec![
            LlmEvent::TextDelta("Hel".into()),
            LlmEvent::TextDelta("lo".into()),
            LlmEvent::Incomplete {
                terminal: LlmTerminal {
                    usage: Some(usage),
                    response_id: Some("msg_01abc".into()),
                },
                reason: "max_tokens".into(),
            },
        ]
    );

    let mut events = vec![message_start()];
    events.extend(message_end("end_turn", 9));
    assert_eq!(
        parse(&events),
        vec![LlmEvent::Completed(LlmTerminal {
            usage: Some(UsageTokens {
                output_tokens: 9,
                ..usage
            }),
            response_id: Some("msg_01abc".into()),
        })]
    );
}

#[test]
fn code_execution_tool_events_named_code_interpreter() {
    let mut events = vec![
        message_start(),
        a(
            "content_block_start",
            json!({"index": 0, "content_block": {"type": "server_tool_use", "id": "srvtoolu_1", "name": "bash_code_execution", "input": {}}}),
        ),
        a(
            "content_block_delta",
            json!({"index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"command\":\"ls\"}"}}),
        ),
        a("content_block_stop", json!({"index": 0})),
        a(
            "content_block_start",
            json!({"index": 1, "content_block": {"type": "bash_code_execution_tool_result", "tool_use_id": "srvtoolu_1",
                   "content": {"type": "bash_code_execution_result", "stdout": "a.txt", "stderr": "", "return_code": 0}}}),
        ),
        a("content_block_stop", json!({"index": 1})),
        a(
            "content_block_start",
            json!({"index": 2, "content_block": {"type": "server_tool_use", "id": "srvtoolu_2", "name": "web_search", "input": {}}}),
        ),
        a("content_block_stop", json!({"index": 2})),
        a(
            "content_block_start",
            json!({"index": 3, "content_block": {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_2", "content": []}}),
        ),
        a("content_block_stop", json!({"index": 3})),
    ];
    events.extend(message_end("end_turn", 5));
    let parsed = parse(&events);
    assert_eq!(
        &parsed[..4],
        &[
            LlmEvent::ToolStart {
                name: "code_interpreter".into(),
                details: json!({}),
            },
            LlmEvent::ToolDone {
                name: "code_interpreter".into(),
                details: json!({}),
            },
            LlmEvent::ToolStart {
                name: "web_search".into(),
                details: json!({}),
            },
            LlmEvent::ToolDone {
                name: "web_search".into(),
                details: json!({}),
            },
        ]
    );
    assert!(matches!(parsed[4], LlmEvent::Completed(_)));
    assert_eq!(parsed.len(), 5);
}

#[test]
fn tool_use_becomes_function_call_with_start_event() {
    let mut events = vec![
        message_start(),
        a(
            "content_block_start",
            json!({"index": 0, "content_block": {"type": "tool_use", "id": "toolu_7", "name": "search_knowledge", "input": {}}}),
        ),
        a(
            "content_block_delta",
            json!({"index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"query\": "}}),
        ),
        a(
            "content_block_delta",
            json!({"index": 0, "delta": {"type": "input_json_delta", "partial_json": "\"vacation\"}"}}),
        ),
        a("content_block_stop", json!({"index": 0})),
        a(
            "content_block_start",
            json!({"index": 1, "content_block": {"type": "tool_use", "id": "toolu_8", "name": "load_files", "input": {}}}),
        ),
        a("content_block_stop", json!({"index": 1})),
        a(
            "content_block_start",
            json!({"index": 2, "content_block": {"type": "tool_use", "id": "toolu_9", "name": "other_tool", "input": {}}}),
        ),
    ];
    events.extend(message_end("tool_use", 5));
    let parsed = parse(&events);
    assert_eq!(
        &parsed[..5],
        &[
            LlmEvent::ToolStart {
                name: "search_knowledge".into(),
                details: json!({}),
            },
            LlmEvent::FunctionCall {
                call_id: "toolu_7".into(),
                name: "search_knowledge".into(),
                arguments: "{\"query\": \"vacation\"}".into(),
            },
            LlmEvent::ToolStart {
                name: "load_files".into(),
                details: json!({}),
            },
            LlmEvent::FunctionCall {
                call_id: "toolu_8".into(),
                name: "load_files".into(),
                arguments: "{}".into(),
            },
            LlmEvent::ToolStart {
                name: "unknown_tool".into(),
                details: json!({}),
            },
        ]
    );
    assert!(matches!(parsed[5], LlmEvent::Completed(_)));
}

#[test]
fn error_event_is_sanitized_failure() {
    let parsed = parse(&[a(
        "error",
        json!({"error": {"type": "overloaded_error", "message": "Overloaded, see msg_01abcdef"}}),
    )]);
    let [LlmEvent::Failed(f)] = parsed.as_slice() else {
        panic!("expected one failure: {parsed:?}");
    };
    assert_eq!(f.code, StreamErrorCode::ProviderError);
    assert_eq!(f.message, "Overloaded, see [provider_id]");
}

#[test]
fn completion_parses_text_and_usage() {
    let c = AnthropicMessagesAdapter
        .parse_completion(&json!({
            "id": "msg_9", "type": "message", "role": "assistant",
            "content": [{"type": "text", "text": "SUM"}, {"type": "text", "text": "MARY"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 3},
        }))
        .unwrap();
    assert_eq!(c.text, "SUMMARY");
    assert_eq!(c.response_id.as_deref(), Some("msg_9"));
    assert_eq!(c.usage.unwrap().input_tokens, 10);

    let err = AnthropicMessagesAdapter
        .parse_completion(
            &json!({"type": "error", "error": {"type": "api_error", "message": "boom"}}),
        )
        .unwrap_err();
    assert_eq!(err.message, "boom");
}
