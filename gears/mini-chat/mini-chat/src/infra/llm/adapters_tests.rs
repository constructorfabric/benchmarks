#![allow(clippy::unwrap_used)]

use serde_json::{Value, json};

use super::anthropic::{self, AnthropicTranslator};
use super::client::build_body;
use super::openai_chat::ChatTranslator;
use super::types::{
    ContentPart, FunctionItem, InputMessage, InputRole, LlmEvent, LlmRequest, RequestMetadata,
    ToolSpec,
};
use crate::config::ProviderKind;

fn req(tools: Vec<ToolSpec>) -> LlmRequest {
    LlmRequest {
        model: "m".into(),
        instructions: "sys".into(),
        input: vec![
            InputMessage::text(InputRole::User, "hi"),
            InputMessage::text(InputRole::Assistant, "hello"),
            InputMessage {
                role: InputRole::User,
                content: vec![
                    ContentPart::Text("look".into()),
                    ContentPart::Image {
                        file_id: "file-1".into(),
                        secondary_file_id: Some("file_ant".into()),
                    },
                ],
            },
        ],
        function_items: vec![
            FunctionItem::Call {
                call_id: "c1".into(),
                name: "search_knowledge".into(),
                arguments: "{\"query\":\"q\"}".into(),
            },
            FunctionItem::Output {
                call_id: "c1".into(),
                output: "out".into(),
            },
        ],
        tools,
        max_output_tokens: 100,
        max_tool_calls: Some(2),
        api_params: mini_chat_sdk::ModelApiParams::default(),
        user: "u".repeat(64),
        metadata: RequestMetadata {
            tenant_id: "t".into(),
            user_id: "u".into(),
            chat_id: "c".into(),
            request_type: "chat".into(),
            feature: "none".into(),
        },
        stream: true,
    }
}

fn all_tools() -> Vec<ToolSpec> {
    vec![
        ToolSpec::FileSearch {
            vector_store_ids: vec!["vs".into()],
            max_num_results: 3,
        },
        ToolSpec::WebSearch {
            search_context_size: "low".into(),
        },
        ToolSpec::CodeInterpreter {
            file_ids: vec!["f".into()],
        },
        ToolSpec::Function {
            name: "search_knowledge".into(),
            description: "d".into(),
            parameters: json!({"type": "object"}),
        },
    ]
}

#[test]
fn chat_completions_body() {
    let (body, headers) = build_body(ProviderKind::OpenaiChatCompletions, &req(all_tools()));
    assert!(headers.is_empty());
    assert_eq!(
        body["messages"][0],
        json!({"role": "system", "content": "sys"})
    );
    assert_eq!(
        body["messages"][1],
        json!({"role": "user", "content": "hi"})
    );
    assert_eq!(body["messages"][3]["content"][1]["type"], "file");
    // only function tools survive
    let tools = body["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["function"]["name"], "search_knowledge");
    assert_eq!(body["max_completion_tokens"], 100);
    assert_eq!(body["stream_options"]["include_usage"], true);
    assert!(body.get("max_tool_calls").is_none());
    let last = body["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(last["role"], "tool");
}

#[test]
fn vllm_body_has_no_tools() {
    let (body, _) = build_body(ProviderKind::VllmResponses, &req(all_tools()));
    assert!(body.get("tools").is_none() && body.get("metadata").is_none());
}

#[test]
fn anthropic_body_and_headers() {
    let (body, headers) = build_body(ProviderKind::AnthropicMessages, &req(all_tools()));
    assert!(headers.contains(&("anthropic-version", anthropic::ANTHROPIC_VERSION)));
    assert!(headers.contains(&("anthropic-beta", anthropic::ANTHROPIC_FILES_BETA)));
    assert_eq!(body["system"], "sys");
    assert_eq!(body["metadata"]["user_id"], "u".repeat(64));
    let names: Vec<&str> = body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["web_search", "code_execution", "search_knowledge"]
    );
    let img = &body["messages"][2]["content"][1];
    assert_eq!(
        img["source"],
        json!({"type": "file", "file_id": "file_ant"})
    );
}

fn feed_chat(t: &mut ChatTranslator, chunks: &[Value]) -> Vec<LlmEvent> {
    chunks
        .iter()
        .flat_map(|c| t.on_event(None, &c.to_string()))
        .collect()
}

#[test]
fn chat_translator_text_usage_and_tool_calls() {
    let mut t = ChatTranslator::new();
    let mut out = feed_chat(
        &mut t,
        &[
            json!({"id": "chatcmpl-1", "choices": [{"delta": {"content": "Hi"}}]}),
            json!({"choices": [{"delta": {}, "finish_reason": "length"}]}),
            json!({"choices": [], "usage": {"prompt_tokens": 3, "completion_tokens": 4}}),
        ],
    );
    out.extend(t.on_event(None, "[DONE]"));
    assert_eq!(out[0], LlmEvent::TextDelta("Hi".into()));
    let LlmEvent::Completed(c) = out.last().unwrap() else {
        panic!("completion expected")
    };
    assert_eq!(c.incomplete_reason.as_deref(), Some("max_tokens"));
    assert_eq!(c.usage.unwrap().input_tokens, 3);

    let mut t = ChatTranslator::new();
    let mut out = feed_chat(
        &mut t,
        &[
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "call_1", "function": {"name": "search_knowledge", "arguments": "{\"q"}}]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": "\":1}"}}]}, "finish_reason": "tool_calls"}]}),
        ],
    );
    out.extend(t.on_event(None, "[DONE]"));
    assert!(matches!(&out[0], LlmEvent::ToolStart { name, .. } if name == "function_call"));
    assert!(
        matches!(&out[1], LlmEvent::ToolDone { name, details } if name == "function_call" && details["arguments"] == "{\"q\":1}")
    );
    assert!(matches!(&out[2], LlmEvent::FunctionCall { call_id, .. } if call_id == "call_1"));
    // an error chunk fails the stream
    let mut t = ChatTranslator::new();
    let out = t.on_event(None, &json!({"error": {"message": "bad"}}).to_string());
    assert!(matches!(&out[0], LlmEvent::Failed(f) if f.message == "bad"));
    // end of stream without a terminal event
    let mut t = ChatTranslator::new();
    assert!(matches!(t.finish(), Some(LlmEvent::Failed(_))));
}

#[test]
fn anthropic_translator_flow() {
    let mut t = AnthropicTranslator::new();
    let evs = [
        (
            "message_start",
            json!({"type": "message_start", "message": {"id": "msg_1", "usage": {"input_tokens": 10}}}),
        ),
        (
            "content_block_start",
            json!({"index": 0, "content_block": {"type": "server_tool_use", "name": "web_search"}}),
        ),
        ("content_block_stop", json!({"index": 0})),
        (
            "content_block_start",
            json!({"index": 1, "content_block": {"type": "text", "text": ""}}),
        ),
        (
            "content_block_delta",
            json!({"index": 1, "delta": {"type": "text_delta", "text": "Hello"}}),
        ),
        (
            "message_delta",
            json!({"delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 5}}),
        ),
        ("message_stop", json!({})),
    ];
    let out: Vec<LlmEvent> = evs
        .iter()
        .flat_map(|(n, d)| t.on_event(Some(n), &d.to_string()))
        .collect();
    assert!(matches!(&out[0], LlmEvent::ToolStart { name, .. } if name == "web_search"));
    assert!(matches!(&out[1], LlmEvent::ToolDone { name, .. } if name == "web_search"));
    assert_eq!(out[2], LlmEvent::TextDelta("Hello".into()));
    let LlmEvent::Completed(c) = &out[3] else {
        panic!("completion expected")
    };
    let u = c.usage.unwrap();
    assert_eq!((u.input_tokens, u.output_tokens), (10, 5));
    // error event
    let mut t = AnthropicTranslator::new();
    let out = t.on_event(
        Some("error"),
        &json!({"error": {"type": "rate_limit_error", "message": "slow"}}).to_string(),
    );
    assert!(matches!(&out[0], LlmEvent::Failed(f) if f.kind.code() == "rate_limited"));
}
