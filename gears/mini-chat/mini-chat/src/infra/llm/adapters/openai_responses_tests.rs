use mini_chat_sdk::{ModelApiParams, WebSearchContextSize};
use serde_json::json;

use super::*;
use crate::infra::llm::types::{RequestMetadata, Role};

fn request(tools: Vec<ToolSpec>) -> LlmRequest {
    LlmRequest {
        model: "gpt-x".into(),
        instructions: "SYS".into(),
        input: vec![
            InputItem::text(Role::User, "q1"),
            InputItem::text(Role::Assistant, "a1"),
            InputItem::Message {
                role: Role::User,
                content: vec![
                    ContentPart::Text("look".into()),
                    ContentPart::Image {
                        file_id: "file-img".into(),
                        secondary_file_id: None,
                    },
                ],
            },
        ],
        max_output_tokens: 512,
        tools,
        max_tool_calls: 2,
        api_params: ModelApiParams::default(),
        user: "u".repeat(64),
        metadata: RequestMetadata {
            tenant_id: "t".into(),
            user_id: "u".into(),
            chat_id: "c".into(),
            request_type: "chat",
            feature: "none".into(),
        },
        stream: true,
    }
}

#[test]
fn body_contains_controlled_fields() {
    let body = build_body(&request(vec![]), true, true);
    assert_eq!(body["model"], "gpt-x");
    assert_eq!(body["instructions"], "SYS");
    assert_eq!(body["stream"], true);
    assert_eq!(body["store"], false);
    assert_eq!(body["max_output_tokens"], 512);
    assert_eq!(body["user"].as_str().unwrap().len(), 64);
    assert_eq!(body["metadata"]["request_type"], "chat");
    assert!(body.get("tools").is_none());
    assert!(body.get("max_tool_calls").is_none());
    let input = body["input"].as_array().unwrap();
    assert_eq!(input[0], json!({"role": "user", "content": "q1"}));
    assert_eq!(input[1]["role"], "assistant");
    assert_eq!(
        input[2]["content"],
        json!([{"type": "input_text", "text": "look"}, {"type": "input_image", "file_id": "file-img"}])
    );
}

#[test]
fn body_tools() {
    let body = build_body(
        &request(vec![
            ToolSpec::FileSearch {
                vector_store_ids: vec!["vs_1".into()],
                max_num_results: 5,
            },
            ToolSpec::WebSearch {
                context_size: WebSearchContextSize::Low,
            },
            ToolSpec::CodeInterpreter {
                file_ids: vec!["file-x".into()],
            },
        ]),
        true,
        true,
    );
    let tools = body["tools"].as_array().unwrap();
    assert_eq!(
        tools[0],
        json!({"type": "file_search", "vector_store_ids": ["vs_1"], "max_num_results": 5})
    );
    assert_eq!(tools[1]["type"], "web_search");
    assert_eq!(tools[1]["search_context_size"], "low");
    assert_eq!(
        tools[2],
        json!({"type": "code_interpreter", "container": {"type": "auto", "file_ids": ["file-x"]}})
    );
    assert_eq!(body["max_tool_calls"], 2);
    assert_eq!(body["include"], json!(["code_interpreter_call.outputs"]));
}

#[test]
fn extra_body_cannot_override_controlled_keys() {
    let mut req = request(vec![]);
    let mut extra = serde_json::Map::new();
    extra.insert("model".into(), json!("evil"));
    extra.insert("store".into(), json!(true));
    extra.insert("seed".into(), json!(7));
    req.api_params.extra_body = Some(extra);
    req.api_params.temperature = Some(0.5);
    let body = build_body(&req, true, true);
    assert_eq!(body["model"], "gpt-x");
    assert_eq!(body["store"], false);
    assert_eq!(body["seed"], 7);
    assert_eq!(body["temperature"], 0.5);
}

#[test]
fn usage_parsing() {
    let u = parse_usage(Some(&json!({
        "input_tokens": 10,
        "output_tokens": 5,
        "input_tokens_details": {"cached_tokens": 3},
        "output_tokens_details": {"reasoning_tokens": 2}
    })))
    .unwrap();
    assert_eq!(
        (
            u.input_tokens,
            u.output_tokens,
            u.cache_read_input_tokens,
            u.reasoning_tokens
        ),
        (10, 5, 3, 2)
    );
    assert!(parse_usage(None).is_none());
    assert!(parse_usage(Some(&json!(null))).is_none());
}

fn push_all(p: &mut ResponsesParser, events: &[(&str, serde_json::Value)]) -> Vec<ProviderEvent> {
    let mut out = Vec::new();
    for (name, data) in events {
        out.extend(p.push(Some(name), &data.to_string()));
    }
    out
}

#[test]
fn stream_text_tools_and_completion() {
    let mut p = ResponsesParser::new(false);
    let out = push_all(
        &mut p,
        &[
            ("response.created", json!({"type": "response.created"})),
            ("response.web_search_call.searching", json!({})),
            ("response.web_search_call.completed", json!({})),
            (
                "response.output_text.delta",
                json!({"delta": "Hello ", "output_index": 1, "content_index": 0}),
            ),
            (
                "response.output_text.delta",
                json!({"delta": "world", "output_index": 1, "content_index": 0}),
            ),
            (
                "response.output_text.annotation.added",
                json!({"output_index": 1, "content_index": 0, "annotation": {"type": "url_citation", "url": "https://e.com", "title": "E", "start_index": 0, "end_index": 5}}),
            ),
            (
                "response.completed",
                json!({"response": {"id": "resp_1", "usage": {"input_tokens": 4, "output_tokens": 2}}}),
            ),
            ("response.output_text.delta", json!({"delta": "ignored"})),
        ],
    );
    assert_eq!(
        out[0],
        ProviderEvent::ToolStart {
            name: "web_search".into(),
            details: json!({})
        }
    );
    assert_eq!(
        out[1],
        ProviderEvent::ToolDone {
            name: "web_search".into(),
            details: json!({})
        }
    );
    assert_eq!(out[2], ProviderEvent::TextDelta("Hello ".into()));
    assert_eq!(out[3], ProviderEvent::TextDelta("world".into()));
    match &out[4] {
        ProviderEvent::Completed {
            response_id,
            usage,
            citations,
            incomplete_reason,
        } => {
            assert_eq!(response_id.as_deref(), Some("resp_1"));
            assert_eq!(usage.unwrap().input_tokens, 4);
            assert!(incomplete_reason.is_none());
            // streamed annotation resolved against the streamed text
            assert_eq!(
                citations,
                &vec![RawCitation::Web {
                    url: "https://e.com".into(),
                    title: "E".into(),
                    snippet: "Hello".into(),
                    span: Some((0, 5)),
                }]
            );
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(out.len(), 5, "nothing after the terminal event");
}

#[test]
fn event_name_falls_back_to_type_field() {
    let mut p = ResponsesParser::new(false);
    let out = p.push(
        None,
        &json!({"type": "response.output_text.delta", "delta": "x"}).to_string(),
    );
    assert_eq!(out, vec![ProviderEvent::TextDelta("x".into())]);
    let out = p.push(
        Some("message"),
        &json!({"type": "response.output_text.delta", "delta": "y"}).to_string(),
    );
    assert_eq!(out, vec![ProviderEvent::TextDelta("y".into())]);
}

#[test]
fn incomplete_is_a_completion_with_reason() {
    let mut p = ResponsesParser::new(false);
    let out = p.push(
        Some("response.incomplete"),
        &json!({"response": {"incomplete_details": {"reason": "max_output_tokens"}, "usage": {"input_tokens": 1, "output_tokens": 1}}}).to_string(),
    );
    assert!(
        matches!(&out[0], ProviderEvent::Completed { incomplete_reason: Some(r), .. } if r == "max_output_tokens")
    );
}

#[test]
fn failure_is_sanitized_and_keeps_usage() {
    let mut p = ResponsesParser::new(false);
    let out = p.push(
        Some("response.failed"),
        &json!({"response": {"error": {"message": "bad file-abcdefghijklmnop at https://x.y/z"}, "usage": {"input_tokens": 3, "output_tokens": 0}}}).to_string(),
    );
    match &out[0] {
        ProviderEvent::Failed(f) => {
            assert_eq!(f.code, ProviderErrorCode::ProviderError);
            assert_eq!(f.message, "bad [provider_id] at [url]");
            assert_eq!(f.usage.unwrap().input_tokens, 3);
        }
        other => panic!("unexpected {other:?}"),
    }
    let mut p = ResponsesParser::new(false);
    let out = p.push(Some("error"), r#"{"code":"x","message":"flat"}"#);
    assert!(matches!(&out[0], ProviderEvent::Failed(f) if f.message == "flat"));
    let mut p = ResponsesParser::new(false);
    let out = p.push(Some("error"), "not json");
    assert!(matches!(&out[0], ProviderEvent::Failed(f) if f.message == "not json"));
}

#[test]
fn file_search_and_code_interpreter_events() {
    let mut p = ResponsesParser::new(false);
    let long_logs = "x".repeat(9000);
    let out = push_all(
        &mut p,
        &[
            ("response.file_search_call.searching", json!({})),
            (
                "response.file_search_call.completed",
                json!({"results": [1, 2]}),
            ),
            ("response.code_interpreter_call.in_progress", json!({})),
            (
                "response.output_item.done",
                json!({"item": {"type": "code_interpreter_call", "outputs": [{"type": "logs", "logs": "a"}, {"type": "image"}, {"type": "logs", "logs": "b"}]}}),
            ),
            (
                "response.output_item.done",
                json!({"item": {"type": "code_interpreter_call", "outputs": [{"type": "logs", "logs": long_logs}]}}),
            ),
            (
                "response.output_item.done",
                json!({"item": {"type": "message"}}),
            ),
        ],
    );
    assert_eq!(
        out[0],
        ProviderEvent::ToolStart {
            name: "file_search".into(),
            details: json!({})
        }
    );
    assert_eq!(
        out[1],
        ProviderEvent::ToolDone {
            name: "file_search".into(),
            details: json!({"files_searched": 2})
        }
    );
    assert_eq!(
        out[2],
        ProviderEvent::ToolStart {
            name: "code_interpreter".into(),
            details: json!({})
        }
    );
    assert_eq!(
        out[3],
        ProviderEvent::ToolDone {
            name: "code_interpreter".into(),
            details: json!({"output": "a\nb"})
        }
    );
    match &out[4] {
        ProviderEvent::ToolDone { details, .. } => {
            let s = details["output"].as_str().unwrap();
            assert!(s.ends_with("...[truncated]"));
            assert_eq!(s.chars().count(), 8192 + "...[truncated]".len());
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(out.len(), 5);
}

#[test]
fn citations_from_completed_output() {
    let resp = json!({"output": [
        {"type": "web_search_call"},
        {"type": "message", "content": [{"type": "output_text", "text": "h\u{e9}llo world", "annotations": [
            {"type": "url_citation", "url": "https://a", "title": "A", "start_index": 0, "end_index": 5},
            {"type": "file_citation", "file_id": "file-1", "filename": "doc.pdf", "index": 3},
            {"type": "url_citation", "url": "https://b", "title": "B", "start_index": 50, "end_index": 60},
            {"type": "unknown"}
        ]}]}
    ]});
    let c = citations_from_output(&resp);
    assert_eq!(c.len(), 3);
    assert_eq!(
        c[0],
        RawCitation::Web {
            url: "https://a".into(),
            title: "A".into(),
            snippet: "h\u{e9}llo".into(),
            span: Some((0, 5))
        }
    );
    assert_eq!(
        c[1],
        RawCitation::File {
            file_id: "file-1".into(),
            filename: Some("doc.pdf".into()),
            span: None
        }
    );
    // out-of-range span -> empty snippet
    assert!(matches!(&c[2], RawCitation::Web { snippet, .. } if snippet.is_empty()));
    assert_eq!(output_text(&resp), "h\u{e9}llo world");
}
