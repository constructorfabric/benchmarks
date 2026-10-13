use mini_chat_sdk::UsageTokens;
use serde_json::json;

use super::*;
use crate::infra::llm::providers::test_util::{
    TENANT, USER, all_tools, ev, knowledge_tool, parse_all, raw, req_with,
};
use crate::infra::llm::sanitize::provider_user_field;
use crate::infra::llm::types::{
    ContentPart, InputMessage, LlmTerminal, Role, StreamErrorCode, ToolSpec,
};

fn chunk(v: &serde_json::Value) -> SseEvent {
    ev(None, v)
}

fn parse(events: &[SseEvent]) -> Vec<LlmEvent> {
    parse_all(&OpenAiChatCompletionsAdapter, events)
}

#[test]
fn body_uses_messages_and_max_completion_tokens() {
    let b = OpenAiChatCompletionsAdapter.build_body(&req_with(Vec::new()));
    assert_eq!(b["model"], "model-x");
    assert_eq!(b["stream"], true);
    assert_eq!(b["stream_options"], json!({"include_usage": true}));
    assert_eq!(b["max_completion_tokens"], 4096);
    assert_eq!(b["user"], provider_user_field(TENANT, USER));
    assert_eq!(
        b["messages"],
        json!([
            {"role": "system", "content": "SYSTEM PROMPT"},
            {"role": "user", "content": "first question"},
            {"role": "assistant", "content": "first answer"},
            {"role": "user", "content": "current question"},
        ])
    );
    for absent in [
        "input",
        "instructions",
        "max_output_tokens",
        "max_tokens",
        "max_tool_calls",
        "metadata",
        "store",
        "tools",
    ] {
        assert!(b.get(absent).is_none(), "{absent} must not be sent: {b}");
    }

    // The non-streaming summary call carries no stream options.
    let mut summary = req_with(Vec::new());
    summary.stream = false;
    let b = OpenAiChatCompletionsAdapter.build_body(&summary);
    assert_eq!(b["stream"], false);
    assert!(b.get("stream_options").is_none());
}

#[test]
fn drops_builtin_tools_keeps_function() {
    let b = OpenAiChatCompletionsAdapter.build_body(&req_with(all_tools()));
    assert_eq!(
        b["tools"],
        json!([{"type": "function", "function": {
            "name": "search_knowledge",
            "description": "Search the knowledge base",
            "parameters": {"type": "object", "properties": {"query": {"type": "string"}}},
        }}])
    );
    assert!(b.get("include").is_none());

    let only_builtin: Vec<ToolSpec> = all_tools()
        .into_iter()
        .filter(|t| !matches!(t, ToolSpec::Function { .. }))
        .collect();
    let b = OpenAiChatCompletionsAdapter.build_body(&req_with(only_builtin));
    assert!(b.get("tools").is_none(), "built-in tools dropped: {b}");
}

#[test]
fn body_replays_function_call_and_output_and_drops_images() {
    let mut req = req_with(vec![knowledge_tool()]);
    req.input[2].content.push(ContentPart::Image {
        file_id: "file-abcdefghijklmnop".into(),
    });
    req.input.push(InputMessage {
        role: Role::Assistant,
        content: vec![ContentPart::FunctionCall {
            call_id: "call_1".into(),
            name: "search_knowledge".into(),
            arguments: r#"{"query":"vacation"}"#.into(),
        }],
    });
    req.input.push(InputMessage {
        role: Role::User,
        content: vec![ContentPart::FunctionOutput {
            call_id: "call_1".into(),
            output: "RESULTS".into(),
        }],
    });
    let b = OpenAiChatCompletionsAdapter.build_body(&req);
    let messages = b["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 6);
    assert_eq!(
        messages[3],
        json!({"role": "user", "content": "current question"})
    );
    assert_eq!(
        messages[4],
        json!({"role": "assistant", "content": null, "tool_calls": [{
            "id": "call_1", "type": "function",
            "function": {"name": "search_knowledge", "arguments": r#"{"query":"vacation"}"#},
        }]})
    );
    assert_eq!(
        messages[5],
        json!({"role": "tool", "tool_call_id": "call_1", "content": "RESULTS"})
    );
}

#[test]
fn parses_deltas_usage_finish_length_incomplete() {
    let events = parse(&[
        chunk(
            &json!({"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}, "finish_reason": null}]}),
        ),
        chunk(
            &json!({"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {"content": "Hel"}, "finish_reason": null}]}),
        ),
        chunk(
            &json!({"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {"content": "lo"}, "finish_reason": null}]}),
        ),
        chunk(
            &json!({"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {}, "finish_reason": "length"}]}),
        ),
        chunk(&json!({"id": "chatcmpl-1", "choices": [], "usage": {
            "prompt_tokens": 11, "completion_tokens": 7,
            "prompt_tokens_details": {"cached_tokens": 4},
            "completion_tokens_details": {"reasoning_tokens": 2},
        }})),
        raw("[DONE]"),
    ]);
    assert_eq!(
        events,
        vec![
            LlmEvent::TextDelta("Hel".into()),
            LlmEvent::TextDelta("lo".into()),
            LlmEvent::Incomplete {
                terminal: LlmTerminal {
                    usage: Some(UsageTokens {
                        input_tokens: 11,
                        output_tokens: 7,
                        cache_read_input_tokens: 4,
                        cache_write_input_tokens: 0,
                        reasoning_tokens: 2,
                    }),
                    response_id: Some("chatcmpl-1".into()),
                },
                reason: "max_tokens".into(),
            },
        ]
    );

    // finish_reason stop → completed.
    let events = parse(&[
        chunk(
            &json!({"id": "chatcmpl-2", "choices": [{"index": 0, "delta": {"content": "ok"}, "finish_reason": "stop"}]}),
        ),
        raw("[DONE]"),
    ]);
    assert_eq!(
        events.last(),
        Some(&LlmEvent::Completed(LlmTerminal {
            usage: None,
            response_id: Some("chatcmpl-2".into()),
        }))
    );
}

#[test]
fn tool_call_events_named_function_call() {
    let events = parse(&[
        chunk(
            &json!({"id": "chatcmpl-3", "choices": [{"index": 0, "delta": {"tool_calls": [
            {"index": 0, "id": "call_9", "type": "function", "function": {"name": "search_knowledge", "arguments": ""}}
        ]}, "finish_reason": null}]}),
        ),
        chunk(
            &json!({"id": "chatcmpl-3", "choices": [{"index": 0, "delta": {"tool_calls": [
            {"index": 0, "function": {"arguments": "{\"query\":"}}
        ]}, "finish_reason": null}]}),
        ),
        chunk(
            &json!({"id": "chatcmpl-3", "choices": [{"index": 0, "delta": {"tool_calls": [
            {"index": 0, "function": {"arguments": "\"x\"}"}}
        ]}, "finish_reason": null}]}),
        ),
        chunk(
            &json!({"id": "chatcmpl-3", "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
        ),
        raw("[DONE]"),
    ]);
    assert_eq!(
        events,
        vec![
            LlmEvent::ToolStart {
                name: "function_call".into(),
                details: json!({"index": 0, "call_id": "call_9", "name": "search_knowledge"}),
            },
            LlmEvent::ToolDone {
                name: "function_call".into(),
                details: json!({"call_id": "call_9", "name": "search_knowledge", "arguments": "{\"query\":\"x\"}"}),
            },
            LlmEvent::FunctionCall {
                call_id: "call_9".into(),
                name: "search_knowledge".into(),
                arguments: "{\"query\":\"x\"}".into(),
            },
            LlmEvent::Completed(LlmTerminal {
                usage: None,
                response_id: Some("chatcmpl-3".into()),
            }),
        ]
    );
}

#[test]
fn error_chunk_is_sanitized_failure() {
    let events = parse(&[chunk(
        &json!({"error": {"message": "bad key sk-abcdefghijklmnop", "type": "invalid_request_error"}}),
    )]);
    let [LlmEvent::Failed(f)] = events.as_slice() else {
        panic!("expected one failure: {events:?}");
    };
    assert_eq!(f.code, StreamErrorCode::ProviderError);
    assert_eq!(f.message, "bad key [credential]");
}

#[test]
fn completion_parses_message_and_usage() {
    let c = OpenAiChatCompletionsAdapter
        .parse_completion(&json!({
            "id": "chatcmpl-9",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "SUMMARY"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 30, "completion_tokens": 8},
        }))
        .unwrap();
    assert_eq!(c.text, "SUMMARY");
    assert_eq!(c.response_id.as_deref(), Some("chatcmpl-9"));
    assert_eq!(c.usage.unwrap().input_tokens, 30);

    let err = OpenAiChatCompletionsAdapter
        .parse_completion(&json!({"error": {"message": "overloaded"}}))
        .unwrap_err();
    assert_eq!(err.message, "overloaded");
}
