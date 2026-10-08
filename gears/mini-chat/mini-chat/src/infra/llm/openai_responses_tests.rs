#![allow(clippy::unwrap_used)]

use serde_json::json;

use super::*;
use crate::infra::llm::types::{InputMessage, RequestMetadata};

fn request(tools: Vec<ToolSpec>) -> LlmRequest {
    LlmRequest {
        model: "gpt-test".into(),
        instructions: "be nice".into(),
        input: vec![
            InputMessage::text(InputRole::User, "hi"),
            InputMessage::text(InputRole::Assistant, "hello"),
            InputMessage {
                role: InputRole::User,
                content: vec![
                    ContentPart::Text("look".into()),
                    ContentPart::Image {
                        file_id: "file-img".into(),
                        secondary_file_id: None,
                    },
                ],
            },
        ],
        function_items: Vec::new(),
        tools,
        max_output_tokens: 1000,
        max_tool_calls: Some(2),
        api_params: mini_chat_sdk::ModelApiParams::default(),
        user: "t:u".into(),
        metadata: RequestMetadata {
            tenant_id: "t".into(),
            user_id: "u".into(),
            chat_id: "c".into(),
            request_type: "chat".into(),
            feature: "file_search".into(),
        },
        stream: true,
    }
}

#[test]
fn build_request_shapes_input_tools_and_metadata() {
    let body = build_request(
        &request(vec![
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
        ]),
        BuildOptions::OPENAI,
    );
    assert_eq!(body["model"], "gpt-test");
    assert_eq!(body["instructions"], "be nice");
    assert_eq!(body["stream"], true);
    assert_eq!(body["max_output_tokens"], 1000);
    assert_eq!(body["user"], "t:u");
    assert_eq!(body["metadata"]["request_type"], "chat");
    assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
    assert_eq!(body["input"][1]["content"][0]["type"], "output_text");
    assert_eq!(body["input"][2]["content"][1]["type"], "input_image");
    assert_eq!(body["input"][2]["content"][1]["file_id"], "file-img");
    assert_eq!(body["tools"][0]["type"], "file_search");
    assert_eq!(body["tools"][0]["vector_store_ids"][0], "vs_1");
    assert_eq!(body["tools"][0]["max_num_results"], 5);
    assert_eq!(body["tools"][1]["search_context_size"], "low");
    assert_eq!(body["tools"][2]["container"]["file_ids"][0], "file-x");
    assert_eq!(body["max_tool_calls"], 2);
    assert_eq!(body["include"][0], "code_interpreter_call.outputs");
}

#[test]
fn vllm_omits_tools_and_metadata() {
    let body = build_request(
        &request(vec![ToolSpec::WebSearch {
            search_context_size: "low".into(),
        }]),
        BuildOptions::VLLM,
    );
    assert!(body.get("tools").is_none());
    assert!(body.get("metadata").is_none());
    assert!(body.get("max_tool_calls").is_none());
}

#[test]
fn extra_body_cannot_override_reserved_keys() {
    let mut req = request(Vec::new());
    let mut extra = serde_json::Map::new();
    extra.insert("model".into(), json!("evil"));
    extra.insert("seed".into(), json!(7));
    req.api_params.extra_body = Some(extra.into_iter().collect());
    req.api_params.temperature = Some(0.5);
    req.api_params.reasoning_effort = Some("low".into());
    let body = build_request(&req, BuildOptions::OPENAI);
    assert_eq!(body["model"], "gpt-test");
    assert_eq!(body["seed"], 7);
    assert_eq!(body["temperature"], 0.5);
    assert_eq!(body["reasoning"]["effort"], "low");
}

fn feed(t: &mut ResponsesTranslator, events: &[Value]) -> Vec<LlmEvent> {
    events
        .iter()
        .flat_map(|e| t.on_event(None, &e.to_string()))
        .collect()
}

#[test]
fn text_and_completion() {
    let mut t = ResponsesTranslator::new(false);
    let out = feed(
        &mut t,
        &[
            json!({"type": "response.created", "response": {"id": "resp_1"}}),
            json!({"type": "response.output_text.delta", "item_id": "m", "delta": "Hel"}),
            json!({"type": "response.output_text.delta", "item_id": "m", "delta": "lo"}),
            json!({"type": "response.completed", "response": {"id": "resp_1",
                "usage": {"input_tokens": 10, "output_tokens": 3,
                          "input_tokens_details": {"cached_tokens": 4}}}}),
        ],
    );
    assert_eq!(out[0], LlmEvent::TextDelta("Hel".into()));
    assert_eq!(out[1], LlmEvent::TextDelta("lo".into()));
    let LlmEvent::Completed(c) = &out[2] else {
        panic!("expected completion")
    };
    let usage = c.usage.unwrap();
    assert_eq!((usage.input_tokens, usage.output_tokens), (10, 3));
    assert_eq!(usage.cache_read_input_tokens, 4);
    assert_eq!(c.response_id.as_deref(), Some("resp_1"));
    assert!(t.finish().is_none());
}

#[test]
fn tool_events_are_deduplicated() {
    let mut t = ResponsesTranslator::new(false);
    let out = feed(
        &mut t,
        &[
            json!({"type": "response.output_item.added", "item": {"type": "web_search_call", "id": "ws_1"}}),
            json!({"type": "response.web_search_call.in_progress", "item_id": "ws_1"}),
            json!({"type": "response.web_search_call.searching", "item_id": "ws_1"}),
            json!({"type": "response.web_search_call.completed", "item_id": "ws_1"}),
            json!({"type": "response.output_item.done", "item": {"type": "web_search_call", "id": "ws_1"}}),
        ],
    );
    let starts = out
        .iter()
        .filter(|e| matches!(e, LlmEvent::ToolStart { name, .. } if name == "web_search"))
        .count();
    let dones = out
        .iter()
        .filter(|e| matches!(e, LlmEvent::ToolDone { name, .. } if name == "web_search"))
        .count();
    assert_eq!((starts, dones), (1, 1));
}

#[test]
fn code_interpreter_output_is_capped() {
    let mut t = ResponsesTranslator::new(false);
    let long = "x".repeat(CODE_OUTPUT_CAP + 10);
    let out = feed(
        &mut t,
        &[json!({"type": "response.output_item.done", "item": {
            "type": "code_interpreter_call", "id": "ci_1",
            "outputs": [{"type": "logs", "logs": long}]}})],
    );
    let LlmEvent::ToolDone { details, .. } = out.last().unwrap() else {
        panic!("expected tool done")
    };
    let output = details["output"].as_str().unwrap();
    assert!(output.ends_with("...[truncated]"));
    assert_eq!(
        output.chars().count(),
        CODE_OUTPUT_CAP + "...[truncated]".len()
    );
}

#[test]
fn annotations_are_collected_once() {
    let mut t = ResponsesTranslator::new(false);
    let ann = json!({"type": "file_citation", "file_id": "file-1", "filename": "a.pdf"});
    let out = feed(
        &mut t,
        &[
            json!({"type": "response.output_text.annotation.added", "item_id": "m", "annotation": ann}),
            json!({"type": "response.output_item.done", "item": {"type": "message",
                "content": [{"type": "output_text", "text": "t", "annotations": [ann,
                    {"type": "url_citation", "url": "https://x", "title": "X", "start_index": 0, "end_index": 1}]}]}}),
        ],
    );
    let anns: Vec<_> = out
        .iter()
        .filter_map(|e| match e {
            LlmEvent::Annotation(a) => Some(a.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(anns.len(), 2);
    assert!(
        matches!(&anns[1], Annotation::Url { url, part_text: Some(p), .. } if url == "https://x" && p == "t")
    );
}

#[test]
fn failure_and_incomplete() {
    let mut t = ResponsesTranslator::new(false);
    let out = feed(
        &mut t,
        &[
            json!({"type": "response.failed", "response": {"id": "r", "error": {"code": "server_error", "message": "boom"}}}),
        ],
    );
    let LlmEvent::Failed(f) = &out[0] else {
        panic!("expected failure")
    };
    assert_eq!(f.provider_code.as_deref(), Some("server_error"));
    assert_eq!(f.message, "boom");

    let mut t = ResponsesTranslator::new(false);
    let out = feed(
        &mut t,
        &[
            json!({"type": "response.incomplete", "response": {"incomplete_details": {"reason": "max_output_tokens"}}}),
        ],
    );
    assert!(
        matches!(&out[0], LlmEvent::Completed(c) if c.incomplete_reason.as_deref() == Some("max_output_tokens"))
    );

    let mut t = ResponsesTranslator::new(false);
    assert!(matches!(t.finish(), Some(LlmEvent::Failed(_))));
}

#[test]
fn vllm_think_blocks_become_reasoning() {
    let mut t = ResponsesTranslator::new(true);
    let out = feed(
        &mut t,
        &[
            json!({"type": "response.output_text.delta", "delta": "<thi"}),
            json!({"type": "response.output_text.delta", "delta": "nk>plan</th"}),
            json!({"type": "response.output_text.delta", "delta": "ink>Answer"}),
            json!({"type": "response.completed", "response": {}}),
        ],
    );
    let reasoning: String = out
        .iter()
        .filter_map(|e| match e {
            LlmEvent::ReasoningDelta(r) => Some(r.as_str()),
            _ => None,
        })
        .collect();
    let text: String = out
        .iter()
        .filter_map(|e| match e {
            LlmEvent::TextDelta(r) => Some(r.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(reasoning, "plan");
    assert_eq!(text, "Answer");
}

#[test]
fn parse_complete_reads_output_text() {
    let (text, usage) = parse_complete(&json!({
        "output": [{"type": "message", "content": [{"type": "output_text", "text": "sum"}]}],
        "usage": {"input_tokens": 5, "output_tokens": 2}
    }));
    assert_eq!(text, "sum");
    assert_eq!(usage.unwrap().output_tokens, 2);
}
