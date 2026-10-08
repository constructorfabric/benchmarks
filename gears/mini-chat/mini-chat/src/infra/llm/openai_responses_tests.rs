use serde_json::json;

use super::*;
use crate::infra::llm::sse::SseFrame;
use crate::infra::llm::{FileSearchTool, InputMessage, RequestTools};

fn frame(event: &str, data: serde_json::Value) -> SseFrame {
    SseFrame { event: Some(event.into()), data: data.to_string() }
}

fn request() -> ChatRequest {
    ChatRequest {
        model: "gpt-x".into(),
        instructions: "sys".into(),
        input: vec![
            InputMessage { role: "user", text: "q1".into(), image_file_ids: vec![] },
            InputMessage { role: "assistant", text: "a1".into(), image_file_ids: vec![] },
            InputMessage { role: "user", text: "q2".into(), image_file_ids: vec!["file-img".into()] },
        ],
        tools: RequestTools {
            file_search: Some(FileSearchTool { vector_store_id: "vs_1".into(), max_num_results: 5 }),
            web_search: Some("low".into()),
            code_interpreter: Some(vec!["file-x".into()]),
            max_tool_calls: Some(2),
        },
        max_output_tokens: 100,
        user: "u".into(),
        metadata: serde_json::Map::new(),
        api_params: mini_chat_sdk::ApiParams { temperature: Some(0.5), ..Default::default() },
        stream: true,
    }
}

#[test]
fn body_shape() {
    let b = build_body(&request(), true, true, true);
    assert_eq!(b["model"], "gpt-x");
    assert_eq!(b["instructions"], "sys");
    assert_eq!(b["stream"], true);
    assert_eq!(b["max_output_tokens"], 100);
    assert_eq!(b["input"][2]["content"][1]["file_id"], "file-img");
    assert_eq!(b["input"][1]["role"], "assistant");
    let tools = b["tools"].as_array().unwrap();
    assert_eq!(tools[0]["vector_store_ids"][0], "vs_1");
    assert_eq!(tools[1]["type"], "web_search");
    assert_eq!(tools[2]["container"]["file_ids"][0], "file-x");
    assert_eq!(b["include"][0], "code_interpreter_call.outputs");
    assert_eq!(b["max_tool_calls"], 2);
    assert_eq!(b["temperature"], 0.5);
    assert!(b.get("top_p").is_none());
}

#[test]
fn decodes_text_tools_and_completion() {
    let mut d = ResponsesDecoder::default();
    let mut evs = Vec::new();
    evs.extend(d.on_frame(&frame("response.created", json!({"response": {"id": "resp_1"}}))));
    evs.extend(d.on_frame(&frame("response.web_search_call.searching", json!({}))));
    evs.extend(d.on_frame(&frame("response.web_search_call.completed", json!({}))));
    evs.extend(d.on_frame(&frame("response.output_text.delta", json!({"delta": "Hi"}))));
    evs.extend(d.on_frame(&frame(
        "response.output_text.annotation.added",
        json!({"annotation": {"type": "url_citation", "url": "https://x", "title": "X", "start_index": 0, "end_index": 2}}),
    )));
    evs.extend(d.on_frame(&frame(
        "response.completed",
        json!({"response": {"id": "resp_1", "usage": {"input_tokens": 5, "output_tokens": 1, "input_tokens_details": {"cached_tokens": 2}}}}),
    )));
    assert!(matches!(&evs[0], ProviderEvent::ResponseId(id) if id == "resp_1"));
    assert!(matches!(&evs[1], ProviderEvent::ToolStart { name, .. } if name == "web_search"));
    assert!(matches!(&evs[2], ProviderEvent::ToolDone { name, .. } if name == "web_search"));
    assert!(matches!(&evs[3], ProviderEvent::TextDelta(t) if t == "Hi"));
    assert!(matches!(&evs[4], ProviderEvent::Citation(RawCitation::Url { .. })));
    match &evs[5] {
        ProviderEvent::Completed { usage: Some(u), incomplete_reason: None, .. } => {
            assert_eq!((u.input_tokens, u.output_tokens, u.cache_read_input_tokens), (5, 1, 2));
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(d.is_terminal());
}

#[test]
fn event_name_from_type_field_and_failures() {
    let mut d = ResponsesDecoder::default();
    let evs = d.on_frame(&SseFrame { event: None, data: json!({"type": "response.output_text.delta", "delta": "a"}).to_string() });
    assert!(matches!(&evs[0], ProviderEvent::TextDelta(t) if t == "a"));
    let evs = d.on_frame(&frame("response.failed", json!({"response": {"error": {"message": "boom"}, "usage": {"input_tokens": 3, "output_tokens": 0}}})));
    match &evs[0] {
        ProviderEvent::Failed { message, usage: Some(u), .. } => {
            assert_eq!(message, "boom");
            assert_eq!(u.input_tokens, 3);
        }
        other => panic!("unexpected {other:?}"),
    }
    let mut d = ResponsesDecoder::default();
    let evs = d.on_frame(&SseFrame { event: Some("error".into()), data: "not json".into() });
    assert!(matches!(&evs[0], ProviderEvent::Failed { message, .. } if message == "not json"));
}

#[test]
fn code_interpreter_output_and_incomplete() {
    let mut d = ResponsesDecoder::default();
    let evs = d.on_frame(&frame(
        "response.output_item.done",
        json!({"item": {"type": "code_interpreter_call", "outputs": [{"type": "logs", "logs": "a"}, {"type": "logs", "logs": "b"}]}}),
    ));
    assert!(matches!(&evs[0], ProviderEvent::ToolDone { details, .. } if details["output"] == "a\nb"));
    let evs = d.on_frame(&frame("response.incomplete", json!({"response": {"incomplete_details": {"reason": "max_output_tokens"}}})));
    assert!(matches!(&evs[0], ProviderEvent::Completed { incomplete_reason: Some(r), .. } if r == "max_output_tokens"));
}
