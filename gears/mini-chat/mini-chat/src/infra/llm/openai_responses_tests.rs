use super::*;
use mini_chat_sdk::ApiParams;

fn frame(event: &str, data: &Value) -> SseFrame {
    SseFrame {
        event: Some(event.to_owned()),
        data: data.to_string(),
    }
}

fn request() -> LlmRequest {
    let mut metadata = Map::new();
    metadata.insert("request_type".to_owned(), json!("chat"));
    LlmRequest {
        model: "gpt-4.1".to_owned(),
        instructions: "be nice".to_owned(),
        input: vec![
            InputMessage {
                role: InputRole::Assistant,
                parts: vec![InputPart::Text("earlier".to_owned())],
                is_current: false,
            },
            InputMessage {
                role: InputRole::User,
                parts: vec![
                    InputPart::Text("what is this?".to_owned()),
                    InputPart::Image {
                        file_id: "file-img".to_owned(),
                    },
                ],
                is_current: true,
            },
        ],
        tools: vec![
            ToolSpec::FileSearch {
                vector_store_ids: vec!["vs_1".to_owned()],
                max_num_results: 5,
            },
            ToolSpec::WebSearch {
                search_context_size: "low".to_owned(),
            },
            ToolSpec::CodeInterpreter {
                file_ids: vec!["file-x".to_owned()],
            },
        ],
        max_output_tokens: 100,
        max_tool_calls: Some(2),
        user: "u".repeat(64),
        metadata,
        api_params: ApiParams {
            temperature: Some(0.5),
            ..ApiParams::default()
        },
        stream: true,
    }
}

#[test]
fn request_shape_matches_responses_api() {
    let body = build_request(&request(), true, true);
    assert_eq!(body["model"], "gpt-4.1");
    assert_eq!(body["instructions"], "be nice");
    assert_eq!(body["stream"], true);
    assert_eq!(body["max_output_tokens"], 100);
    assert_eq!(body["max_tool_calls"], 2);
    assert_eq!(body["temperature"], 0.5);
    assert_eq!(body["input"][0], json!({"role": "assistant", "content": "earlier"}));
    assert_eq!(
        body["input"][1]["content"],
        json!([{"type": "input_text", "text": "what is this?"}, {"type": "input_image", "file_id": "file-img"}])
    );
    assert_eq!(body["tools"][0]["type"], "file_search");
    assert_eq!(body["tools"][0]["vector_store_ids"], json!(["vs_1"]));
    assert_eq!(body["tools"][1], json!({"type": "web_search", "search_context_size": "low"}));
    assert_eq!(body["tools"][2]["container"]["file_ids"], json!(["file-x"]));
    assert_eq!(body["include"], json!(["code_interpreter_call.outputs"]));
    assert_eq!(body["metadata"]["request_type"], "chat");
    let no_tools = build_request(&request(), false, false);
    assert!(no_tools.get("tools").is_none());
    assert!(no_tools.get("metadata").is_none());
}

#[test]
fn extra_body_cannot_override_controlled_keys() {
    let mut req = request();
    let mut extra = Map::new();
    extra.insert("model".to_owned(), json!("other"));
    extra.insert("seed".to_owned(), json!(7));
    req.api_params.extra_body = Some(extra);
    let body = build_request(&req, true, true);
    assert_eq!(body["model"], "gpt-4.1");
    assert_eq!(body["seed"], 7);
}

#[test]
#[allow(clippy::cognitive_complexity)]
fn translates_text_tools_and_completion() {
    let mut t = ResponsesTranslator::new();
    let mut events = Vec::new();
    events.extend(t.on_frame(&frame("response.created", &json!({"type": "response.created"}))));
    events.extend(t.on_frame(&frame(
        "response.output_text.delta",
        &json!({"delta": "Hello", "output_index": 0, "content_index": 0}),
    )));
    events.extend(t.on_frame(&frame("response.web_search_call.searching", &json!({}))));
    events.extend(t.on_frame(&frame("response.web_search_call.completed", &json!({}))));
    events.extend(t.on_frame(&frame(
        "response.file_search_call.completed",
        &json!({"results": [1, 2]}),
    )));
    events.extend(t.on_frame(&frame(
        "response.output_item.done",
        &json!({"item": {"type": "code_interpreter_call", "outputs": [{"type": "logs", "logs": "a"}, {"type": "logs", "logs": "b"}]}}),
    )));
    events.extend(t.on_frame(&frame(
        "response.completed",
        &json!({"response": {"id": "resp_1", "usage": {"input_tokens": 10, "output_tokens": 5, "input_tokens_details": {"cached_tokens": 3}, "output_tokens_details": {"reasoning_tokens": 1}}}}),
    )));
    assert_eq!(events[0], LlmEvent::TextDelta("Hello".to_owned()));
    assert!(matches!(&events[1], LlmEvent::ToolStart { name, .. } if name == "web_search"));
    assert!(matches!(&events[2], LlmEvent::ToolDone { name, .. } if name == "web_search"));
    assert!(
        matches!(&events[3], LlmEvent::ToolDone { name, details } if name == "file_search" && details["files_searched"] == 2)
    );
    assert!(
        matches!(&events[4], LlmEvent::ToolDone { name, details } if name == "code_interpreter" && details["output"] == "a\nb")
    );
    match &events[5] {
        LlmEvent::Completed {
            usage: Some(u),
            response_id,
            incomplete_reason,
        } => {
            assert_eq!((u.input_tokens, u.output_tokens), (10, 5));
            assert_eq!(u.cache_read_input_tokens, 3);
            assert_eq!(u.reasoning_tokens, 1);
            assert_eq!(response_id.as_deref(), Some("resp_1"));
            assert!(incomplete_reason.is_none());
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(t.is_terminal());
}

#[test]
fn event_name_falls_back_to_type_field() {
    let mut t = ResponsesTranslator::new();
    let ev = t.on_frame(&SseFrame {
        event: None,
        data: json!({"type": "response.output_text.delta", "delta": "x"}).to_string(),
    });
    assert_eq!(ev, vec![LlmEvent::TextDelta("x".to_owned())]);
}

#[test]
fn web_citation_snippet_from_range_and_file_citation() {
    let mut t = ResponsesTranslator::new();
    t.on_frame(&frame(
        "response.output_text.delta",
        &json!({"delta": "Rust is fast and safe", "output_index": 1, "content_index": 0}),
    ));
    let ev = t.on_frame(&frame(
        "response.output_text.annotation.added",
        &json!({"output_index": 1, "content_index": 0, "annotation": {"type": "url_citation", "url": "https://r.example", "title": "R", "start_index": 8, "end_index": 12}}),
    ));
    assert_eq!(
        ev,
        vec![LlmEvent::Citation(RawCitation::Web {
            url: "https://r.example".to_owned(),
            title: "R".to_owned(),
            snippet: "fast".to_owned(),
            span: Some((8, 12)),
        })]
    );
    let ev = t.on_frame(&frame(
        "response.output_text.annotation.added",
        &json!({"annotation": {"type": "file_citation", "file_id": "file-abc", "filename": "a.pdf", "index": 3}}),
    ));
    assert_eq!(
        ev,
        vec![LlmEvent::Citation(RawCitation::File {
            file_id: "file-abc".to_owned(),
            filename: Some("a.pdf".to_owned())
        })]
    );
}

#[test]
fn citations_from_final_response_when_no_annotation_events() {
    let mut t = ResponsesTranslator::new();
    let ev = t.on_frame(&frame(
        "response.completed",
        &json!({"response": {"output": [{"type": "message", "content": [{"type": "output_text", "text": "see docs", "annotations": [{"type": "url_citation", "url": "https://d.example", "title": "D", "start_index": 4, "end_index": 8}]}]}]}}),
    ));
    assert!(matches!(&ev[0], LlmEvent::Citation(RawCitation::Web { snippet, .. }) if snippet == "docs"));
    assert!(matches!(&ev[1], LlmEvent::Completed { usage: None, .. }));
}

#[test]
fn failure_events() {
    let mut t = ResponsesTranslator::new();
    let ev = t.on_frame(&frame(
        "response.failed",
        &json!({"response": {"error": {"message": "boom file-ABCDEFGHIJKLMN"}, "usage": {"input_tokens": 7, "output_tokens": 0}}}),
    ));
    match &ev[0] {
        LlmEvent::Failed(f) => {
            assert_eq!(f.kind, ProviderErrorKind::ProviderError);
            assert_eq!(f.message, "boom file-ABCDEFGHIJKLMN");
            assert_eq!(f.usage.map(|u| u.input_tokens), Some(7));
        }
        other => panic!("unexpected {other:?}"),
    }
    let mut t = ResponsesTranslator::new();
    let ev = t.on_frame(&frame("error", &json!({"code": "x", "message": "bad"})));
    assert!(matches!(&ev[0], LlmEvent::Failed(f) if f.message == "bad"));
    let mut t = ResponsesTranslator::new();
    let ev = t.on_frame(&SseFrame {
        event: Some("error".to_owned()),
        data: "not json".to_owned(),
    });
    assert!(matches!(&ev[0], LlmEvent::Failed(f) if f.message == "not json"));
}

#[test]
fn incomplete_is_a_completion_with_reason() {
    let mut t = ResponsesTranslator::new();
    let ev = t.on_frame(&frame(
        "response.incomplete",
        &json!({"response": {"incomplete_details": {"reason": "max_output_tokens"}, "usage": {"input_tokens": 1, "output_tokens": 2}}}),
    ));
    assert!(
        matches!(&ev[0], LlmEvent::Completed { incomplete_reason: Some(r), .. } if r == "max_output_tokens")
    );
}
