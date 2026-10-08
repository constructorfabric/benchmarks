#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

fn frame(event: &str, data: &Value) -> SseFrame {
    SseFrame {
        event: Some(event.to_owned()),
        data: data.to_string(),
    }
}

#[test]
fn text_delta_and_completed_usage() {
    let mut t = ResponsesTranslator::new();
    let e = t.translate(&frame(
        "response.output_text.delta",
        &json!({"type":"response.output_text.delta","delta":"Hel"}),
    ));
    assert_eq!(e, vec![LlmEvent::Text("Hel".into())]);
    let e = t.translate(&frame(
        "response.completed",
        &json!({"type":"response.completed","response":{"id":"resp_1","usage":{"input_tokens":10,"output_tokens":3,
            "input_tokens_details":{"cached_tokens":4},"output_tokens_details":{"reasoning_tokens":1}},"output":[]}}),
    ));
    match &e[0] {
        LlmEvent::Completed(c) => {
            let u = c.usage.unwrap();
            assert_eq!(
                (
                    u.input_tokens,
                    u.output_tokens,
                    u.cache_read_input_tokens,
                    u.reasoning_tokens
                ),
                (10, 3, 4, 1)
            );
            assert_eq!(c.response_id.as_deref(), Some("resp_1"));
            assert!(c.incomplete_reason.is_none());
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn event_name_from_type_when_missing() {
    let mut t = ResponsesTranslator::new();
    let f = SseFrame {
        event: None,
        data: json!({"type":"response.output_text.delta","delta":"x"}).to_string(),
    };
    assert_eq!(t.translate(&f), vec![LlmEvent::Text("x".into())]);
    let f = SseFrame {
        event: Some("message".into()),
        data: json!({"type":"response.output_text.delta","delta":"y"}).to_string(),
    };
    assert_eq!(t.translate(&f), vec![LlmEvent::Text("y".into())]);
}

#[test]
fn tool_events() {
    let mut t = ResponsesTranslator::new();
    assert_eq!(
        t.translate(&frame("response.file_search_call.searching", &json!({}))),
        vec![LlmEvent::ToolStart {
            name: "file_search"
        }]
    );
    assert_eq!(
        t.translate(&frame(
            "response.file_search_call.completed",
            &json!({"results":[1,2]})
        )),
        vec![LlmEvent::ToolDone {
            name: "file_search",
            details: json!({"files_searched": 2})
        }]
    );
    assert_eq!(
        t.translate(&frame("response.web_search_call.searching", &json!({}))),
        vec![LlmEvent::ToolStart { name: "web_search" }]
    );
    assert_eq!(
        t.translate(&frame("response.web_search_call.completed", &json!({}))),
        vec![LlmEvent::ToolDone {
            name: "web_search",
            details: json!({})
        }]
    );
    assert_eq!(
        t.translate(&frame(
            "response.code_interpreter_call.in_progress",
            &json!({})
        )),
        vec![LlmEvent::ToolStart {
            name: "code_interpreter"
        }]
    );
    let done = t.translate(&frame(
        "response.output_item.done",
        &json!({"item":{"type":"code_interpreter_call","outputs":[{"type":"logs","logs":"a"},{"type":"logs","logs":"b"}]}}),
    ));
    assert_eq!(
        done,
        vec![LlmEvent::ToolDone {
            name: "code_interpreter",
            details: json!({"output":"a\nb"})
        }]
    );
    assert!(
        t.translate(&frame(
            "response.code_interpreter_call.completed",
            &json!({})
        ))
        .is_empty()
    );
    assert_eq!(
        t.translate(&frame(
            "response.output_item.added",
            &json!({"item":{"type":"function_call","name":"search_knowledge"}})
        )),
        vec![LlmEvent::UnexpectedToolUse("search_knowledge".into())]
    );
}

#[test]
fn citations_from_completed_response() {
    let mut t = ResponsesTranslator::new();
    let text = "See the market report for details.";
    let e = t.translate(&frame(
        "response.completed",
        &json!({"response":{"output":[{"type":"message","content":[{"type":"output_text","text":text,"annotations":[
            {"type":"url_citation","url":"https://ex.com/a","title":"A","start_index":8,"end_index":21},
            {"type":"file_citation","file_id":"file-abc","filename":"r.pdf","index":3}
        ]}]}]}}),
    ));
    let LlmEvent::Completed(c) = &e[0] else {
        panic!()
    };
    assert_eq!(c.citations.len(), 2);
    assert_eq!(
        c.citations[0],
        RawCitation::Web {
            url: "https://ex.com/a".into(),
            title: "A".into(),
            snippet: "market report".into(),
            span: Some((8, 21))
        }
    );
    assert_eq!(
        c.citations[1],
        RawCitation::File {
            file_id: "file-abc".into(),
            filename: "r.pdf".into()
        }
    );
}

#[test]
fn incomplete_is_completion_without_citations() {
    let mut t = ResponsesTranslator::new();
    let e = t.translate(&frame(
        "response.incomplete",
        &json!({"response":{"incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":1,"output_tokens":2}}}),
    ));
    let LlmEvent::Completed(c) = &e[0] else {
        panic!()
    };
    assert_eq!(c.incomplete_reason.as_deref(), Some("max_output_tokens"));
    assert!(c.citations.is_empty());
}

#[test]
fn failures_are_sanitized() {
    let mut t = ResponsesTranslator::new();
    let e = t.translate(&frame(
        "response.failed",
        &json!({"response":{"error":{"code":"server_error","message":"failed on file-ABCDEFGHIJKLMNOP"},"usage":{"input_tokens":5,"output_tokens":0}}}),
    ));
    let LlmEvent::Failed {
        kind,
        message,
        usage,
    } = &e[0]
    else {
        panic!()
    };
    assert_eq!(*kind, FailKind::ProviderError);
    assert!(!message.contains("file-ABC"));
    assert_eq!(usage.unwrap().input_tokens, 5);
    let e = t.translate(&frame(
        "error",
        &json!({"code":"x","message":"bad resp_123"}),
    ));
    let LlmEvent::Failed { message, .. } = &e[0] else {
        panic!()
    };
    assert_eq!(message, "bad [provider_id]");
    let e = t.translate(&SseFrame {
        event: Some("error".into()),
        data: "not json".into(),
    });
    assert!(matches!(&e[0], LlmEvent::Failed { message, .. } if message == "not json"));
}

fn request(tools: ToolSet) -> ChatRequest {
    let mut metadata = Map::new();
    metadata.insert("request_type".into(), json!("chat"));
    ChatRequest {
        provider_model_id: "gpt-x".into(),
        instructions: "be nice".into(),
        input: vec![
            InputMessage {
                role: InputRole::User,
                text: "hi".into(),
                image_file_ids: vec![],
            },
            InputMessage {
                role: InputRole::Assistant,
                text: "hello".into(),
                image_file_ids: vec![],
            },
            InputMessage {
                role: InputRole::User,
                text: "img?".into(),
                image_file_ids: vec!["file-1".into()],
            },
        ],
        tools,
        max_output_tokens: 100,
        max_tool_calls: 2,
        user: "u".into(),
        metadata,
        api_params: ApiParams {
            temperature: Some(0.5),
            ..ApiParams::default()
        },
        stream: true,
    }
}

#[test]
fn responses_body_shape() {
    let tools = ToolSet {
        file_search: Some((vec!["vs_1".into()], 5)),
        web_search: Some(WebSearchContextSize::Low),
        code_interpreter: Some(vec!["file-x".into()]),
    };
    let b = build_responses_body(&request(tools.clone()), ProviderKind::OpenaiResponses);
    assert_eq!(b["model"], "gpt-x");
    assert_eq!(b["instructions"], "be nice");
    assert_eq!(b["stream"], true);
    assert_eq!(b["max_output_tokens"], 100);
    assert_eq!(b["max_tool_calls"], 2);
    assert_eq!(b["temperature"], 0.5);
    assert_eq!(
        b["input"][0]["content"][0],
        json!({"type":"input_text","text":"hi"})
    );
    assert_eq!(b["input"][1], json!({"role":"assistant","content":"hello"}));
    assert_eq!(
        b["input"][2]["content"][1],
        json!({"type":"input_image","file_id":"file-1"})
    );
    assert_eq!(
        b["tools"][0],
        json!({"type":"file_search","vector_store_ids":["vs_1"],"max_num_results":5})
    );
    assert_eq!(
        b["tools"][1],
        json!({"type":"web_search","search_context_size":"low"})
    );
    assert_eq!(
        b["tools"][2],
        json!({"type":"code_interpreter","container":{"type":"auto","file_ids":["file-x"]}})
    );
    assert_eq!(b["include"], json!(["code_interpreter_call.outputs"]));
    assert_eq!(
        tools.feature_label(),
        "file_search+web_search+code_interpreter"
    );
    let v = build_responses_body(&request(tools), ProviderKind::VllmResponses);
    assert!(v.get("tools").is_none());
    assert!(v.get("metadata").is_none());
    assert!(
        build_responses_body(&request(ToolSet::default()), ProviderKind::OpenaiResponses)
            .get("tools")
            .is_none()
    );
}

#[test]
fn extra_body_controlled_keys_ignored() {
    let mut r = request(ToolSet::default());
    let mut extra = Map::new();
    extra.insert("model".into(), json!("evil"));
    extra.insert("seed".into(), json!(7));
    r.api_params.extra_body = Some(extra);
    let b = build_responses_body(&r, ProviderKind::OpenaiResponses);
    assert_eq!(b["model"], "gpt-x");
    assert_eq!(b["seed"], 7);
}

#[test]
fn chat_completions_translation() {
    let mut t = ChatCompletionsTranslator::default();
    let e = t.translate(&SseFrame {
        event: None,
        data: json!({"id":"c1","choices":[{"delta":{"content":"hi"}}]}).to_string(),
    });
    assert_eq!(e, vec![LlmEvent::Text("hi".into())]);
    t.translate(&SseFrame {
        event: None,
        data: json!({"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":2}}).to_string(),
    });
    let e = t.translate(&SseFrame {
        event: None,
        data: "[DONE]".into(),
    });
    let LlmEvent::Completed(c) = &e[0] else {
        panic!()
    };
    assert_eq!(c.usage.unwrap().input_tokens, 3);
}
