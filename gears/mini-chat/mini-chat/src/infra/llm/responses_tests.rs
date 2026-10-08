use bytes::Bytes;
use mini_chat_sdk::{ModelApiParams, UsageTokens, WebSearchContextSize};
use serde_json::{Value, json};

use super::*;
use crate::config::ProviderKind;
use crate::infra::llm::transport::{HttpResponse, RawSseEvent, TransportError};
use crate::testing::ev;

// ───────────────────────────── helpers ─────────────────────────────

fn base_request() -> ChatRequest {
    ChatRequest {
        model: "gpt-test".into(),
        instructions: "Be helpful".into(),
        input: vec![
            InputItem {
                role: InputRole::User,
                text: "hi".into(),
                image_file_ids: vec![],
            },
            InputItem {
                role: InputRole::Assistant,
                text: "hello".into(),
                image_file_ids: vec![],
            },
            InputItem {
                role: InputRole::User,
                text: "what is this?".into(),
                image_file_ids: vec!["file-img1".into()],
            },
        ],
        max_output_tokens: 1024,
        tools: vec![],
        user: provider_user_field(uuid::Uuid::from_u128(1), uuid::Uuid::from_u128(2)),
        metadata: RequestMetadata {
            tenant_id: "t".into(),
            user_id: "u".into(),
            chat_id: "c".into(),
            request_type: "chat".into(),
            feature: "none".into(),
        },
        api_params: ModelApiParams::default(),
        max_tool_calls: 2,
        stream: true,
    }
}

fn all_tools() -> Vec<ToolSpec> {
    vec![
        ToolSpec::FileSearch {
            vector_store_ids: vec!["vs_1".into()],
            max_num_results: 5,
        },
        ToolSpec::WebSearch {
            context_size: WebSearchContextSize::High,
        },
        ToolSpec::CodeInterpreter {
            file_ids: vec!["file-x".into()],
        },
    ]
}

fn raw(event: Option<&str>, data: &Value) -> RawSseEvent {
    RawSseEvent {
        event: event.map(str::to_owned),
        data: data.to_string(),
    }
}

fn feed(p: &mut StreamParser, events: &[RawSseEvent]) -> Vec<ProviderEvent> {
    events.iter().flat_map(|e| p.on_event(e)).collect()
}

fn http(status: u16, body: &str, retry: Option<u64>) -> HttpResponse {
    HttpResponse {
        status,
        retry_after_secs: retry,
        body: Bytes::from(body.to_owned()),
    }
}

// ───────────────────────────── request body ─────────────────────────────

#[test]
fn responses_body_without_tools() {
    let req = base_request();
    let b = build_request_body(ProviderKind::OpenaiResponses, &req);
    assert_eq!(b["model"], "gpt-test");
    assert_eq!(b["instructions"], "Be helpful");
    assert_eq!(b["stream"], true);
    assert_eq!(b["store"], false);
    assert_eq!(b["max_output_tokens"], 1024);
    assert_eq!(b["user"].as_str().unwrap().len(), 64);
    assert_eq!(
        b["metadata"],
        json!({"tenant_id": "t", "user_id": "u", "chat_id": "c", "request_type": "chat", "feature": "none"})
    );
    for k in [
        "tools",
        "max_tool_calls",
        "include",
        "temperature",
        "top_p",
        "stop",
        "reasoning",
    ] {
        assert!(b.get(k).is_none(), "{k} must be absent");
    }
    assert_eq!(
        b["input"],
        json!([
            {"role": "user", "content": [{"type": "input_text", "text": "hi"}]},
            {"role": "assistant", "content": [{"type": "output_text", "text": "hello"}]},
            {"role": "user", "content": [
                {"type": "input_text", "text": "what is this?"},
                {"type": "input_image", "file_id": "file-img1"}
            ]}
        ])
    );
}

#[test]
fn responses_body_omits_empty_instructions() {
    let mut req = base_request();
    req.instructions.clear();
    let b = build_request_body(ProviderKind::OpenaiResponses, &req);
    assert!(b.get("instructions").is_none());
}

#[test]
fn responses_body_multiple_images() {
    let mut req = base_request();
    req.input = vec![InputItem {
        role: InputRole::User,
        text: "x".into(),
        image_file_ids: vec!["a".into(), "b".into()],
    }];
    let b = build_request_body(ProviderKind::OpenaiResponses, &req);
    let content = b["input"][0]["content"].as_array().unwrap();
    assert_eq!(content.len(), 3);
    assert_eq!(content[2], json!({"type": "input_image", "file_id": "b"}));
}

#[test]
fn responses_body_file_search_only() {
    let mut req = base_request();
    req.tools = vec![ToolSpec::FileSearch {
        vector_store_ids: vec!["vs_1".into()],
        max_num_results: 7,
    }];
    let b = build_request_body(ProviderKind::OpenaiResponses, &req);
    assert_eq!(
        b["tools"],
        json!([{"type": "file_search", "vector_store_ids": ["vs_1"], "max_num_results": 7}])
    );
    assert_eq!(b["max_tool_calls"], 2);
    assert!(b.get("include").is_none());
}

#[test]
fn responses_body_web_search_only() {
    let mut req = base_request();
    req.tools = vec![ToolSpec::WebSearch {
        context_size: WebSearchContextSize::Low,
    }];
    let b = build_request_body(ProviderKind::OpenaiResponses, &req);
    assert_eq!(
        b["tools"],
        json!([{"type": "web_search", "search_context_size": "low"}])
    );
    assert!(b.get("include").is_none());
}

#[test]
fn responses_body_code_interpreter_adds_include() {
    let mut req = base_request();
    req.tools = vec![ToolSpec::CodeInterpreter {
        file_ids: vec!["file-x".into(), "file-y".into()],
    }];
    req.max_tool_calls = 3;
    let b = build_request_body(ProviderKind::OpenaiResponses, &req);
    assert_eq!(
        b["tools"],
        json!([{"type": "code_interpreter", "container": {"type": "auto", "file_ids": ["file-x", "file-y"]}}])
    );
    assert_eq!(b["include"], json!(["code_interpreter_call.outputs"]));
    assert_eq!(b["max_tool_calls"], 3);
}

#[test]
fn responses_body_all_tools() {
    let mut req = base_request();
    req.tools = all_tools();
    let b = build_request_body(ProviderKind::OpenaiResponses, &req);
    let tools = b["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 3);
    assert_eq!(
        tools[1],
        json!({"type": "web_search", "search_context_size": "high"})
    );
    assert_eq!(b["include"], json!(["code_interpreter_call.outputs"]));
}

#[test]
fn api_params_sent_only_when_set() {
    let mut req = base_request();
    req.api_params = ModelApiParams {
        temperature: Some(0.5),
        top_p: None,
        frequency_penalty: Some(0.1),
        presence_penalty: None,
        stop: vec!["END".into()],
        extra_body: None,
        reasoning_effort: Some("low".into()),
    };
    let b = build_request_body(ProviderKind::OpenaiResponses, &req);
    assert_eq!(b["temperature"], 0.5);
    assert_eq!(b["frequency_penalty"], 0.1);
    assert!(b.get("top_p").is_none());
    assert!(b.get("presence_penalty").is_none());
    assert_eq!(b["stop"], json!(["END"]));
    assert_eq!(b["reasoning"], json!({"effort": "low"}));
}

#[test]
fn extra_body_merged_except_controlled_keys() {
    let mut req = base_request();
    let mut extra = serde_json::Map::new();
    extra.insert("service_tier".into(), json!("flex"));
    extra.insert("text".into(), json!({"verbosity": "low"}));
    for k in [
        "model",
        "input",
        "stream",
        "store",
        "tools",
        "include",
        "user",
        "metadata",
        "max_output_tokens",
        "instructions",
    ] {
        extra.insert(k.into(), json!("HACK"));
    }
    req.api_params.extra_body = Some(extra);
    let b = build_request_body(ProviderKind::OpenaiResponses, &req);
    assert_eq!(b["service_tier"], "flex");
    assert_eq!(b["text"], json!({"verbosity": "low"}));
    assert_eq!(b["model"], "gpt-test");
    assert_eq!(b["stream"], true);
    assert_eq!(b["store"], false);
    assert!(b.get("tools").is_none());
    assert!(b.get("include").is_none());
    assert_eq!(b["metadata"]["request_type"], "chat");
    assert_eq!(b["max_output_tokens"], 1024);
    assert_eq!(b["instructions"], "Be helpful");
    assert_ne!(b["user"], "HACK");
}

#[test]
fn vllm_body_drops_tools_and_metadata() {
    let mut req = base_request();
    req.tools = all_tools();
    let b = build_request_body(ProviderKind::VllmResponses, &req);
    for k in ["tools", "include", "max_tool_calls", "metadata"] {
        assert!(b.get(k).is_none(), "{k} must be absent");
    }
    assert_eq!(b["store"], false);
    assert!(b["user"].is_string());
    assert_eq!(b["input"][0]["content"][0]["type"], "input_text");
}

#[test]
fn chat_completions_body() {
    let mut req = base_request();
    req.tools = all_tools();
    let b = build_request_body(ProviderKind::OpenaiChatCompletions, &req);
    assert_eq!(
        b["messages"][0],
        json!({"role": "system", "content": "Be helpful"})
    );
    assert_eq!(b["messages"][1], json!({"role": "user", "content": "hi"}));
    assert_eq!(
        b["messages"][2],
        json!({"role": "assistant", "content": "hello"})
    );
    assert_eq!(b["max_completion_tokens"], 1024);
    assert_eq!(b["stream_options"], json!({"include_usage": true}));
    assert!(b["user"].is_string());
    for k in [
        "tools",
        "include",
        "max_tool_calls",
        "metadata",
        "input",
        "store",
    ] {
        assert!(b.get(k).is_none(), "{k} must be absent");
    }
}

#[test]
fn anthropic_body_maps_tools() {
    let mut req = base_request();
    req.tools = all_tools();
    let mut extra = serde_json::Map::new();
    extra.insert("foo".into(), json!(1));
    req.api_params.extra_body = Some(extra);
    let b = build_request_body(ProviderKind::AnthropicMessages, &req);
    assert_eq!(b["system"], "Be helpful");
    assert_eq!(b["max_tokens"], 1024);
    assert_eq!(b["metadata"], json!({"user_id": req.user}));
    assert_eq!(
        b["messages"][0],
        json!({"role": "user", "content": [{"type": "text", "text": "hi"}]})
    );
    assert_eq!(
        b["tools"],
        json!([
            {"type": "web_search_20250305", "name": "web_search"},
            {"type": "code_execution_20250522", "name": "code_execution"}
        ])
    );
    assert!(b.get("foo").is_none(), "anthropic does not send extra_body");
    assert!(b.get("user").is_none());
}

#[test]
fn anthropic_body_without_supported_tools_has_no_tools() {
    let mut req = base_request();
    req.tools = vec![ToolSpec::FileSearch {
        vector_store_ids: vec!["vs".into()],
        max_num_results: 1,
    }];
    let b = build_request_body(ProviderKind::AnthropicMessages, &req);
    assert!(b.get("tools").is_none());
}

// ───────────────────────────── parser: Responses ─────────────────────────────

#[test]
fn event_name_from_event_line_or_data_type() {
    let mut p = StreamParser::new(ProviderKind::OpenaiResponses);
    // event: line present
    assert_eq!(
        p.on_event(&ev("response.output_text.delta", json!({"delta": "a"}))),
        vec![ProviderEvent::TextDelta("a".into())]
    );
    // no event line → data.type
    let e = raw(
        None,
        &json!({"type": "response.output_text.delta", "delta": "b"}),
    );
    assert_eq!(p.on_event(&e), vec![ProviderEvent::TextDelta("b".into())]);
    // event: message → data.type
    let e = raw(
        Some("message"),
        &json!({"type": "response.output_text.delta", "delta": "c"}),
    );
    assert_eq!(p.on_event(&e), vec![ProviderEvent::TextDelta("c".into())]);
}

#[test]
fn unknown_and_unparseable_events_are_ignored() {
    let mut p = StreamParser::new(ProviderKind::OpenaiResponses);
    assert!(
        p.on_event(&ev(
            "response.created",
            json!({"response": {"id": "resp_1"}})
        ))
        .is_empty()
    );
    assert!(
        p.on_event(&ev("response.something_new", json!({})))
            .is_empty()
    );
    assert!(
        p.on_event(&RawSseEvent {
            event: Some("response.output_text.delta".into()),
            data: "not json".into()
        })
        .is_empty()
    );
    assert!(
        p.on_event(&ev(
            "response.code_interpreter_call.interpreting",
            json!({})
        ))
        .is_empty()
    );
    assert!(
        p.on_event(&ev("response.code_interpreter_call.completed", json!({})))
            .is_empty()
    );
    assert!(
        p.on_event(&ev(
            "response.output_item.done",
            json!({"item": {"type": "message"}})
        ))
        .is_empty()
    );
}

#[test]
fn tool_events() {
    let mut p = StreamParser::new(ProviderKind::OpenaiResponses);
    let out = feed(
        &mut p,
        &[
            ev("response.file_search_call.searching", json!({})),
            ev("response.file_search_call.completed", json!({})),
            ev(
                "response.file_search_call.completed",
                json!({"results": [{}, {}, {}]}),
            ),
            ev("response.web_search_call.searching", json!({})),
            ev("response.web_search_call.completed", json!({})),
            ev("response.code_interpreter_call.in_progress", json!({})),
        ],
    );
    assert_eq!(
        out,
        vec![
            ProviderEvent::ToolStart {
                name: "file_search".into(),
                details: json!({})
            },
            ProviderEvent::ToolDone {
                name: "file_search".into(),
                details: json!({"files_searched": 0})
            },
            ProviderEvent::ToolDone {
                name: "file_search".into(),
                details: json!({"files_searched": 3})
            },
            ProviderEvent::ToolStart {
                name: "web_search".into(),
                details: json!({})
            },
            ProviderEvent::ToolDone {
                name: "web_search".into(),
                details: json!({})
            },
            ProviderEvent::ToolStart {
                name: "code_interpreter".into(),
                details: json!({})
            },
        ]
    );
}

#[test]
fn code_interpreter_done_joins_logs() {
    let mut p = StreamParser::new(ProviderKind::OpenaiResponses);
    let out = p.on_event(&ev(
        "response.output_item.done",
        json!({"item": {"type": "code_interpreter_call", "outputs": [
            {"type": "logs", "logs": "line1"},
            {"type": "image", "url": "x"},
            {"type": "logs", "logs": "line2"}
        ]}}),
    ));
    assert_eq!(
        out,
        vec![ProviderEvent::ToolDone {
            name: "code_interpreter".into(),
            details: json!({"output": "line1\nline2"})
        }]
    );
    let out = p.on_event(&ev(
        "response.output_item.done",
        json!({"item": {"type": "code_interpreter_call"}}),
    ));
    assert_eq!(
        out,
        vec![ProviderEvent::ToolDone {
            name: "code_interpreter".into(),
            details: json!({"output": ""})
        }]
    );
}

#[test]
fn code_interpreter_output_truncated() {
    let mut p = StreamParser::new(ProviderKind::OpenaiResponses);
    let long = "\u{e9}".repeat(9000);
    let out = p.on_event(&ev(
        "response.output_item.done",
        json!({"item": {"type": "code_interpreter_call", "outputs": [{"type": "logs", "logs": long}]}}),
    ));
    let ProviderEvent::ToolDone { details, .. } = &out[0] else {
        panic!("expected tool done")
    };
    let s = details["output"].as_str().unwrap();
    assert!(s.ends_with("...[truncated]"));
    assert_eq!(s.chars().count(), 8192 + "...[truncated]".len());
}

#[test]
fn completed_usage_mapping() {
    let mut p = StreamParser::new(ProviderKind::OpenaiResponses);
    let out = p.on_event(&ev(
        "response.completed",
        json!({"response": {"id": "resp_x", "usage": {"input_tokens": 12, "output_tokens": 34,
            "input_tokens_details": {"cached_tokens": 5}, "output_tokens_details": {"reasoning_tokens": 7}}}}),
    ));
    assert_eq!(
        out,
        vec![ProviderEvent::Completed {
            usage: Some(UsageTokens {
                input_tokens: 12,
                output_tokens: 34,
                cache_read_input_tokens: 5,
                cache_write_input_tokens: 0,
                reasoning_tokens: 7
            }),
            response_id: Some("resp_x".into()),
            incomplete_reason: None,
        }]
    );
    // After the terminal event everything is ignored.
    assert!(
        p.on_event(&ev("response.output_text.delta", json!({"delta": "late"})))
            .is_empty()
    );
}

#[test]
fn completed_without_usage_and_missing_fields() {
    let mut p = StreamParser::new(ProviderKind::OpenaiResponses);
    p.on_event(&ev(
        "response.created",
        json!({"response": {"id": "resp_early"}}),
    ));
    let out = p.on_event(&ev("response.completed", json!({"response": {}})));
    assert_eq!(
        out,
        vec![ProviderEvent::Completed {
            usage: None,
            response_id: Some("resp_early".into()),
            incomplete_reason: None
        }]
    );

    let mut p = StreamParser::new(ProviderKind::OpenaiResponses);
    let out = p.on_event(&ev(
        "response.completed",
        json!({"response": {"usage": {"input_tokens": 3}}}),
    ));
    let ProviderEvent::Completed { usage, .. } = &out[0] else {
        panic!()
    };
    assert_eq!(
        *usage,
        Some(UsageTokens {
            input_tokens: 3,
            ..UsageTokens::default()
        })
    );
}

#[test]
fn incomplete_reason() {
    let mut p = StreamParser::new(ProviderKind::OpenaiResponses);
    let out = p.on_event(&ev(
        "response.incomplete",
        json!({"response": {"id": "resp_i", "incomplete_details": {"reason": "max_output_tokens"},
            "usage": {"input_tokens": 1, "output_tokens": 2}}}),
    ));
    assert_eq!(
        out,
        vec![ProviderEvent::Completed {
            usage: Some(UsageTokens {
                input_tokens: 1,
                output_tokens: 2,
                ..UsageTokens::default()
            }),
            response_id: Some("resp_i".into()),
            incomplete_reason: Some("max_output_tokens".into()),
        }]
    );
    let mut p = StreamParser::new(ProviderKind::OpenaiResponses);
    let out = p.on_event(&ev("response.incomplete", json!({"response": {}})));
    assert!(
        matches!(&out[0], ProviderEvent::Completed { incomplete_reason: Some(r), .. } if r == "other")
    );
}

#[test]
fn response_failed_with_and_without_usage() {
    let mut p = StreamParser::new(ProviderKind::OpenaiResponses);
    let out = p.on_event(&ev(
        "response.failed",
        json!({"response": {"error": {"code": "server_error", "message": "bad thing resp_abc123"},
            "usage": {"input_tokens": 4, "output_tokens": 1}}}),
    ));
    assert_eq!(
        out,
        vec![ProviderEvent::Failed {
            code: "provider_error".into(),
            message: "bad thing resp_abc123".into(),
            usage: Some(UsageTokens {
                input_tokens: 4,
                output_tokens: 1,
                ..UsageTokens::default()
            }),
        }]
    );
    let mut p = StreamParser::new(ProviderKind::OpenaiResponses);
    let out = p.on_event(&ev(
        "response.failed",
        json!({"response": {}, "error": {"message": "top level"}}),
    ));
    assert_eq!(
        out,
        vec![ProviderEvent::Failed {
            code: "provider_error".into(),
            message: "top level".into(),
            usage: None
        }]
    );
    let mut p = StreamParser::new(ProviderKind::OpenaiResponses);
    let out = p.on_event(&ev("response.failed", json!({"response": {}})));
    assert_eq!(
        out,
        vec![ProviderEvent::Failed {
            code: "provider_error".into(),
            message: "provider error".into(),
            usage: None
        }]
    );
}

#[test]
fn error_event_variants() {
    let failed = |m: &str| {
        vec![ProviderEvent::Failed {
            code: "provider_error".into(),
            message: m.into(),
            usage: None,
        }]
    };
    // nested like response.failed
    let mut p = StreamParser::new(ProviderKind::OpenaiResponses);
    assert_eq!(
        p.on_event(&raw(
            Some("error"),
            &json!({"response": {"error": {"message": "nested"}}})
        )),
        failed("nested")
    );
    // top-level error object
    let mut p = StreamParser::new(ProviderKind::OpenaiResponses);
    assert_eq!(
        p.on_event(&raw(Some("error"), &json!({"error": {"message": "obj"}}))),
        failed("obj")
    );
    // flat {type, code, message} without event line
    let mut p = StreamParser::new(ProviderKind::OpenaiResponses);
    assert_eq!(
        p.on_event(&raw(
            None,
            &json!({"type": "error", "code": "rate_limit_exceeded", "message": "flat"})
        )),
        failed("flat")
    );
    // unparseable data
    let mut p = StreamParser::new(ProviderKind::OpenaiResponses);
    assert_eq!(
        p.on_event(&RawSseEvent {
            event: Some("error".into()),
            data: "upstream exploded".into()
        }),
        failed("upstream exploded")
    );
}

#[test]
fn annotation_added_url_with_text_and_range() {
    let mut p = StreamParser::new(ProviderKind::OpenaiResponses);
    let out = feed(
        &mut p,
        &[
            ev(
                "response.output_text.delta",
                json!({"delta": "Hello ", "output_index": 1, "content_index": 0}),
            ),
            ev(
                "response.output_text.delta",
                json!({"delta": "w\u{f6}rld!", "output_index": 1, "content_index": 0}),
            ),
            ev(
                "response.output_text.annotation.added",
                json!({"output_index": 1, "content_index": 0,
                "annotation": {"type": "url_citation", "url": "https://a", "title": "A", "start_index": 6, "end_index": 11}}),
            ),
            ev(
                "response.output_text.annotation.added",
                json!({"output_index": 1, "content_index": 0,
                "annotation": {"type": "url_citation", "url": "https://b", "title": "B", "text": "given",
                    "start_index": 0, "end_index": 5}}),
            ),
            ev(
                "response.output_text.annotation.added",
                json!({"output_index": 1, "content_index": 0,
                "annotation": {"type": "url_citation", "url": "https://c", "title": "C", "start_index": 6, "end_index": 99}}),
            ),
            ev(
                "response.output_text.annotation.added",
                json!({"output_index": 0, "content_index": 0,
                "annotation": {"type": "url_citation", "url": "https://d", "start_index": 0, "end_index": 2}}),
            ),
        ],
    );
    let anns: Vec<_> = out
        .into_iter()
        .filter_map(|e| match e {
            ProviderEvent::Annotation(a) => Some(a),
            _ => None,
        })
        .collect();
    assert_eq!(
        anns,
        vec![
            Annotation::Url {
                url: "https://a".into(),
                title: "A".into(),
                snippet: "w\u{f6}rld".into(),
                start: Some(6),
                end: Some(11)
            },
            Annotation::Url {
                url: "https://b".into(),
                title: "B".into(),
                snippet: "given".into(),
                start: Some(0),
                end: Some(5)
            },
            Annotation::Url {
                url: "https://c".into(),
                title: "C".into(),
                snippet: String::new(),
                start: Some(6),
                end: Some(99)
            },
            Annotation::Url {
                url: "https://d".into(),
                title: String::new(),
                snippet: String::new(),
                start: Some(0),
                end: Some(2)
            },
        ]
    );
}

#[test]
fn file_citation_annotation() {
    let mut p = StreamParser::new(ProviderKind::OpenaiResponses);
    let out = p.on_event(&ev(
        "response.output_text.annotation.added",
        json!({"annotation": {"type": "file_citation", "file_id": "file-abc", "filename": "a.pdf", "index": 3}}),
    ));
    assert_eq!(
        out,
        vec![ProviderEvent::Annotation(Annotation::File {
            file_id: "file-abc".into(),
            filename: Some("a.pdf".into())
        })]
    );
}

fn final_response_with_annotations() -> RawSseEvent {
    ev(
        "response.completed",
        json!({"response": {"id": "resp_f", "output": [
            {"type": "web_search_call"},
            {"type": "message", "content": [{"type": "output_text", "text": "Answer text",
                "annotations": [
                    {"type": "url_citation", "url": "https://u", "title": "U", "start_index": 0, "end_index": 6},
                    {"type": "file_citation", "file_id": "file-1", "filename": "f.pdf"},
                    {"type": "file_citation", "file_id": "file-1", "filename": "f.pdf"}
                ]}]}
        ], "usage": {"input_tokens": 1, "output_tokens": 1}}}),
    )
}

#[test]
fn annotations_extracted_at_completed_when_none_streamed() {
    let mut p = StreamParser::new(ProviderKind::OpenaiResponses);
    let out = p.on_event(&final_response_with_annotations());
    assert_eq!(out.len(), 3, "{out:?}");
    assert_eq!(
        out[0],
        ProviderEvent::Annotation(Annotation::Url {
            url: "https://u".into(),
            title: "U".into(),
            snippet: "Answer".into(),
            start: Some(0),
            end: Some(6)
        })
    );
    assert_eq!(
        out[1],
        ProviderEvent::Annotation(Annotation::File {
            file_id: "file-1".into(),
            filename: Some("f.pdf".into())
        })
    );
    assert!(matches!(out[2], ProviderEvent::Completed { .. }));
}

#[test]
fn no_fallback_extraction_when_annotations_were_streamed() {
    let mut p = StreamParser::new(ProviderKind::OpenaiResponses);
    let added = p.on_event(&ev(
        "response.output_text.annotation.added",
        json!({"annotation": {"type": "url_citation", "url": "https://u", "title": "U", "start_index": 0, "end_index": 6}}),
    ));
    assert_eq!(added.len(), 1);
    let out = p.on_event(&final_response_with_annotations());
    assert_eq!(out.len(), 1);
    assert!(matches!(out[0], ProviderEvent::Completed { .. }));
}

// ───────────────────────────── parser: vLLM ─────────────────────────────

#[test]
fn vllm_think_blocks_become_reasoning() {
    let mut p = StreamParser::new(ProviderKind::VllmResponses);
    let out = feed(
        &mut p,
        &[
            ev("response.output_text.delta", json!({"delta": "<thi"})),
            ev("response.output_text.delta", json!({"delta": "nk>plan"})),
            ev("response.output_text.delta", json!({"delta": " more</th"})),
            ev("response.output_text.delta", json!({"delta": "ink>Answer"})),
            ev("response.output_text.delta", json!({"delta": " <"})),
            ev("response.completed", json!({"response": {}})),
        ],
    );
    assert_eq!(
        out,
        vec![
            ProviderEvent::ReasoningDelta("plan".into()),
            ProviderEvent::ReasoningDelta(" more".into()),
            ProviderEvent::TextDelta("Answer".into()),
            ProviderEvent::TextDelta(" ".into()),
            ProviderEvent::TextDelta("<".into()),
            ProviderEvent::Completed {
                usage: None,
                response_id: None,
                incomplete_reason: None
            },
        ]
    );
}

// ───────────────────────────── parser: Chat Completions ─────────────────────────────

#[test]
fn chat_completions_stream() {
    let mut p = StreamParser::new(ProviderKind::OpenaiChatCompletions);
    let chunk = |v: Value| RawSseEvent {
        event: None,
        data: v.to_string(),
    };
    let out = feed(
        &mut p,
        &[
            chunk(
                json!({"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}}]}),
            ),
            chunk(
                json!({"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {"content": "Hel"}}]}),
            ),
            chunk(
                json!({"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {"content": "lo"}, "finish_reason": "length"}]}),
            ),
            chunk(
                json!({"id": "chatcmpl-1", "choices": [], "usage": {"prompt_tokens": 9, "completion_tokens": 2,
                "prompt_tokens_details": {"cached_tokens": 4}, "completion_tokens_details": {"reasoning_tokens": 1}}}),
            ),
            RawSseEvent {
                event: None,
                data: "[DONE]".into(),
            },
        ],
    );
    assert_eq!(
        out,
        vec![
            ProviderEvent::TextDelta("Hel".into()),
            ProviderEvent::TextDelta("lo".into()),
            ProviderEvent::Completed {
                usage: Some(UsageTokens {
                    input_tokens: 9,
                    output_tokens: 2,
                    cache_read_input_tokens: 4,
                    cache_write_input_tokens: 0,
                    reasoning_tokens: 1
                }),
                response_id: Some("chatcmpl-1".into()),
                incomplete_reason: Some("max_tokens".into()),
            },
        ]
    );
    assert!(p.finish().is_empty());
}

#[test]
fn chat_completions_finish_without_done_and_error() {
    let mut p = StreamParser::new(ProviderKind::OpenaiChatCompletions);
    let chunk = |v: Value| RawSseEvent {
        event: None,
        data: v.to_string(),
    };
    p.on_event(&chunk(
        json!({"choices": [{"delta": {"content": "x"}, "finish_reason": "stop"}]}),
    ));
    assert_eq!(
        p.finish(),
        vec![ProviderEvent::Completed {
            usage: None,
            response_id: None,
            incomplete_reason: None
        }]
    );

    let mut p = StreamParser::new(ProviderKind::OpenaiChatCompletions);
    let out = p.on_event(&chunk(json!({"error": {"message": "boom"}})));
    assert_eq!(
        out,
        vec![ProviderEvent::Failed {
            code: "provider_error".into(),
            message: "boom".into(),
            usage: None
        }]
    );
}

// ───────────────────────────── parser: Anthropic ─────────────────────────────

#[test]
fn anthropic_stream() {
    let mut p = StreamParser::new(ProviderKind::AnthropicMessages);
    let a = |name: &str, v: Value| RawSseEvent {
        event: Some(name.into()),
        data: v.to_string(),
    };
    let out = feed(
        &mut p,
        &[
            a(
                "message_start",
                json!({"type": "message_start", "message": {"id": "msg_1",
                "usage": {"input_tokens": 10, "cache_read_input_tokens": 5, "cache_creation_input_tokens": 2, "output_tokens": 1}}}),
            ),
            a(
                "content_block_start",
                json!({"index": 0, "content_block": {"type": "server_tool_use", "name": "code_execution"}}),
            ),
            a("content_block_stop", json!({"index": 0})),
            a(
                "content_block_start",
                json!({"index": 1, "content_block": {"type": "text", "text": ""}}),
            ),
            a("ping", json!({})),
            a(
                "content_block_delta",
                json!({"index": 1, "delta": {"type": "text_delta", "text": "Hi"}}),
            ),
            a("content_block_stop", json!({"index": 1})),
            a(
                "message_delta",
                json!({"delta": {"stop_reason": "max_tokens"}, "usage": {"output_tokens": 20}}),
            ),
            a("message_stop", json!({})),
        ],
    );
    assert_eq!(
        out,
        vec![
            ProviderEvent::ToolStart {
                name: "code_interpreter".into(),
                details: json!({})
            },
            ProviderEvent::ToolDone {
                name: "code_interpreter".into(),
                details: json!({})
            },
            ProviderEvent::TextDelta("Hi".into()),
            ProviderEvent::Completed {
                usage: Some(UsageTokens {
                    input_tokens: 17,
                    output_tokens: 20,
                    cache_read_input_tokens: 5,
                    cache_write_input_tokens: 2,
                    reasoning_tokens: 0
                }),
                response_id: Some("msg_1".into()),
                incomplete_reason: Some("max_tokens".into()),
            },
        ]
    );
}

#[test]
fn anthropic_error_event() {
    let mut p = StreamParser::new(ProviderKind::AnthropicMessages);
    let out = p.on_event(&RawSseEvent {
        event: Some("error".into()),
        data:
            json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}})
                .to_string(),
    });
    assert_eq!(
        out,
        vec![ProviderEvent::Failed {
            code: "provider_error".into(),
            message: "Overloaded".into(),
            usage: None
        }]
    );
}

// ───────────────────────────── HTTP errors ─────────────────────────────

#[test]
fn http_429_with_and_without_retry_after() {
    let f = map_http_error(&http(429, r#"{"error":{"message":"slow down"}}"#, Some(7)));
    assert_eq!(f.code, "rate_limited");
    assert_eq!(f.message, "Rate limited by provider (retry after 7 s)");
    assert_eq!(f.retry_after_secs, Some(7));
    let f = map_http_error(&http(429, "", None));
    assert_eq!(f.message, "Rate limited by provider");
}

#[test]
fn http_500_with_error_json() {
    let f = map_http_error(&http(
        500,
        r#"{"error":{"message":"The server had an error resp_abc"}}"#,
        None,
    ));
    assert_eq!(f.code, "provider_error");
    assert_eq!(f.message, "The server had an error resp_abc");
    let f = map_http_error(&http(400, r#"{"message":"flat message"}"#, None));
    assert_eq!(f.message, "flat message");
}

#[test]
fn http_504_deadline_problem_vs_provider_504() {
    let problem = r#"{"type":"gts://gts.cf.core.errors.err.v1~cf.core.err.deadline_exceeded.v1~","title":"Deadline Exceeded","status":504}"#;
    let f = map_http_error(&http(504, problem, None));
    assert_eq!(f.code, "provider_timeout");
    let f = map_http_error(&http(
        504,
        r#"{"error":{"message":"upstream timeout"}}"#,
        None,
    ));
    assert_eq!(f.code, "provider_error");
    assert_eq!(f.message, "upstream timeout");
}

#[test]
fn http_error_non_json_body() {
    let f = map_http_error(&http(502, "<html>Bad gateway</html>", None));
    assert_eq!(f.code, "provider_error");
    assert_eq!(f.message, "<html>Bad gateway</html>");
    let long = "x".repeat(800);
    let f = map_http_error(&http(500, &long, None));
    assert_eq!(f.message.len(), 500);
    let f = map_http_error(&http(503, "", None));
    assert_eq!(f.message, "Provider returned HTTP 503");
}

#[test]
fn transport_errors() {
    assert_eq!(
        map_transport_error(&TransportError::Timeout("t".into())).code,
        "provider_timeout"
    );
    assert_eq!(
        map_transport_error(&TransportError::Other("o".into())).code,
        "provider_error"
    );
}

// ───────────────────────────── non-streaming ─────────────────────────────

#[test]
fn parse_completion_responses() {
    let body = json!({"id": "resp_s", "status": "completed", "output": [
        {"type": "reasoning"},
        {"type": "message", "content": [{"type": "output_text", "text": "Part 1 "}, {"type": "output_text", "text": "Part 2"}]}
    ], "usage": {"input_tokens": 100, "output_tokens": 20}});
    let r = parse_completion(
        ProviderKind::OpenaiResponses,
        &http(200, &body.to_string(), None),
    )
    .unwrap();
    assert_eq!(r.text, "Part 1 Part 2");
    assert_eq!(r.response_id.as_deref(), Some("resp_s"));
    assert_eq!(r.usage.unwrap().input_tokens, 100);

    let body = json!({"output_text": "fallback"});
    let r = parse_completion(
        ProviderKind::OpenaiResponses,
        &http(200, &body.to_string(), None),
    )
    .unwrap();
    assert_eq!(r.text, "fallback");
    assert!(r.usage.is_none());
}

#[test]
fn parse_completion_errors() {
    let e = parse_completion(ProviderKind::OpenaiResponses, &http(429, "{}", Some(3))).unwrap_err();
    assert_eq!(e.code, "rate_limited");
    let e = parse_completion(ProviderKind::OpenaiResponses, &http(200, "nope", None)).unwrap_err();
    assert_eq!(e.code, "provider_error");
    let body = json!({"status": "failed", "error": {"message": "bad"}});
    let e = parse_completion(
        ProviderKind::OpenaiResponses,
        &http(200, &body.to_string(), None),
    )
    .unwrap_err();
    assert_eq!(e.message, "bad");
}

#[test]
fn parse_completion_other_kinds() {
    let body = json!({"id": "chatcmpl-9", "choices": [{"message": {"role": "assistant", "content": "cc"}}],
        "usage": {"prompt_tokens": 3, "completion_tokens": 4}});
    let r = parse_completion(
        ProviderKind::OpenaiChatCompletions,
        &http(200, &body.to_string(), None),
    )
    .unwrap();
    assert_eq!(r.text, "cc");
    assert_eq!(r.usage.unwrap().output_tokens, 4);

    let body = json!({"id": "msg_9", "content": [{"type": "text", "text": "an"}, {"type": "text", "text": "th"}],
        "usage": {"input_tokens": 3, "output_tokens": 4}});
    let r = parse_completion(
        ProviderKind::AnthropicMessages,
        &http(200, &body.to_string(), None),
    )
    .unwrap();
    assert_eq!(r.text, "anth");

    let body =
        json!({"output": [{"content": [{"type": "output_text", "text": "<think>x</think>Sum"}]}]});
    let r = parse_completion(
        ProviderKind::VllmResponses,
        &http(200, &body.to_string(), None),
    )
    .unwrap();
    assert_eq!(r.text, "Sum");
}
