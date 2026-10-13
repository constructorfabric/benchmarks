#![allow(clippy::unwrap_used, clippy::expect_used)]

use mini_chat_sdk::ModelApiParams;
use serde_json::{Value, json};
use uuid::Uuid;

use super::OpenAiResponsesAdapter;
use crate::domain::model::MessageRole;
use crate::domain::sanitize::user_field;
use crate::infra::llm::adapter_fixtures::knowledge_round;
use crate::infra::llm::{
    FunctionCall, LlmError, LlmEvent, LlmMessage, LlmRequest, LlmTool, LlmUsage, ParseState,
    ProviderAdapter, RawCitation, RequestMetadata, RequestType,
};

const TENANT: Uuid = Uuid::from_u128(0x0000_0000_df51_5b42_9538_d2b5_6b7e_e953);
const USER: Uuid = Uuid::from_u128(0x1111_1111_6a88_4768_9dfc_6bcd_5187_d9ed);
const CHAT: Uuid = Uuid::from_u128(0x2222_2222_0000_4000_8000_0000_0000_0001);

fn request(tools: Vec<LlmTool>) -> LlmRequest {
    let metadata = RequestMetadata::new(TENANT, USER, CHAT, RequestType::Chat, &tools);
    LlmRequest {
        model: "gpt-5.2".to_owned(),
        instructions: "Be helpful.".to_owned(),
        input: vec![
            LlmMessage::text(MessageRole::User, "earlier question"),
            LlmMessage::text(MessageRole::Assistant, "earlier answer"),
            LlmMessage::text(MessageRole::User, "hi"),
        ],
        max_output_tokens: 4096,
        tools,
        max_tool_calls: Some(2),
        api_params: ModelApiParams::default(),
        user: user_field(&TENANT.to_string(), &USER.to_string()),
        metadata,
        stream: true,
        tool_rounds: Vec::new(),
    }
}

fn body(req: &LlmRequest) -> Value {
    OpenAiResponsesAdapter.build_body(req)
}

#[test]
fn body_has_wire_fields() {
    let req = request(vec![
        LlmTool::FileSearch {
            vector_store_id: "vs_1".to_owned(),
            max_num_results: 5,
        },
        LlmTool::WebSearch {
            context_size: "low".to_owned(),
        },
    ]);
    let b = body(&req);

    assert_eq!(b["model"], json!("gpt-5.2"));
    assert_eq!(b["instructions"], json!("Be helpful."));
    assert_eq!(b["stream"], json!(true));
    assert_eq!(b["store"], json!(false));
    assert_eq!(b["max_output_tokens"], json!(4096));
    assert_eq!(b["user"].as_str().unwrap().len(), 64);
    assert_eq!(
        b["user"],
        json!(format!("{}{}", TENANT.simple(), USER.simple()))
    );
    assert_eq!(
        b["metadata"],
        json!({
            "tenant_id": TENANT.to_string(),
            "user_id": USER.to_string(),
            "chat_id": CHAT.to_string(),
            "request_type": "chat",
            "feature": "file_search+web_search",
        })
    );
    assert_eq!(
        b["tools"],
        json!([
            {"type": "file_search", "vector_store_ids": ["vs_1"], "max_num_results": 5},
            {"type": "web_search", "search_context_size": "low"},
        ])
    );
    assert_eq!(b["max_tool_calls"], json!(2));
    assert!(b.get("include").is_none());
    assert_eq!(
        b["input"],
        json!([
            {"role": "user", "content": "earlier question"},
            {"role": "assistant", "content": "earlier answer"},
            {"role": "user", "content": [{"type": "input_text", "text": "hi"}]},
        ])
    );
}

#[test]
fn code_interpreter_adds_include() {
    let req = request(vec![LlmTool::CodeInterpreter {
        file_ids: vec!["file-x1".to_owned(), "file-x2".to_owned()],
    }]);
    let b = body(&req);
    assert_eq!(
        b["tools"],
        json!([{"type": "code_interpreter", "container": {"type": "auto", "file_ids": ["file-x1", "file-x2"]}}])
    );
    assert_eq!(b["include"], json!(["code_interpreter_call.outputs"]));
    assert_eq!(b["metadata"]["feature"], json!("code_interpreter"));
    assert_eq!(b["max_tool_calls"], json!(2));
}

#[test]
fn no_tools_omits_tools_and_max_tool_calls_and_feature_none() {
    let b = body(&request(Vec::new()));
    assert!(b.get("tools").is_none());
    assert!(b.get("max_tool_calls").is_none());
    assert!(b.get("include").is_none());
    assert_eq!(b["metadata"]["feature"], json!("none"));
}

#[test]
fn function_tool_alone_has_no_max_tool_calls() {
    let b = body(&request(vec![LlmTool::SearchKnowledge]));
    assert_eq!(b["tools"][0]["type"], json!("function"));
    assert_eq!(b["tools"][0]["name"], json!("search_knowledge"));
    assert_eq!(b["tools"][0]["parameters"]["required"], json!(["query"]));
    assert!(b.get("max_tool_calls").is_none());
    assert_eq!(b["metadata"]["feature"], json!("none"));
}

#[test]
fn image_input_content_array() {
    let mut req = request(Vec::new());
    req.input.last_mut().unwrap().image_file_ids = vec!["file-1".to_owned()];
    let b = body(&req);
    assert_eq!(
        b["input"].as_array().unwrap().last().unwrap(),
        &json!({
            "role": "user",
            "content": [
                {"type": "input_text", "text": "hi"},
                {"type": "input_image", "file_id": "file-1"},
            ],
        })
    );
}

#[test]
fn api_params_only_when_set() {
    let b = body(&request(Vec::new()));
    for key in [
        "temperature",
        "top_p",
        "frequency_penalty",
        "presence_penalty",
        "stop",
        "reasoning",
    ] {
        assert!(b.get(key).is_none(), "{key} must be absent");
    }

    let mut req = request(Vec::new());
    req.api_params = ModelApiParams {
        temperature: Some(0.5),
        top_p: Some(0.9),
        frequency_penalty: Some(0.1),
        presence_penalty: Some(0.2),
        stop: vec!["END".to_owned()],
        extra_body: None,
        reasoning_effort: Some("low".to_owned()),
    };
    let b = body(&req);
    assert_eq!(b["temperature"], json!(0.5));
    assert_eq!(b["top_p"], json!(0.9));
    assert_eq!(b["frequency_penalty"], json!(0.1));
    assert_eq!(b["presence_penalty"], json!(0.2));
    assert_eq!(b["stop"], json!(["END"]));
    assert_eq!(b["reasoning"], json!({"effort": "low"}));
}

#[test]
fn extra_body_merged_except_controlled_keys() {
    let mut req = request(Vec::new());
    let extra = json!({"model": "x", "foo": 1, "store": true, "metadata": {}, "user": "u"});
    req.api_params.extra_body = Some(extra.as_object().unwrap().clone());
    let b = body(&req);
    assert_eq!(b["foo"], json!(1));
    assert_eq!(b["model"], json!("gpt-5.2"));
    assert_eq!(b["store"], json!(false));
    assert_eq!(b["metadata"]["request_type"], json!("chat"));
    assert_eq!(b["user"].as_str().unwrap().len(), 64);
}

#[test]
fn summary_request_is_non_streaming() {
    let mut req = request(Vec::new());
    req.stream = false;
    req.metadata = RequestMetadata::new(TENANT, USER, CHAT, RequestType::Summary, &[]);
    let b = body(&req);
    assert_eq!(b["stream"], json!(false));
    assert_eq!(b["metadata"]["request_type"], json!("summary"));
    assert_eq!(b["metadata"]["feature"], json!("none"));
}

// ---------------------------------------------------------------------------
// event translation
// ---------------------------------------------------------------------------

fn feed(events: &[(&str, Value)]) -> Vec<LlmEvent> {
    let mut st = ParseState::default();
    events
        .iter()
        .flat_map(|(name, data)| {
            OpenAiResponsesAdapter.parse_event(&mut st, name, &data.to_string())
        })
        .collect()
}

fn usage(input: i64, output: i64, cached: i64, reasoning: i64) -> Value {
    json!({
        "input_tokens": input,
        "input_tokens_details": {"cached_tokens": cached},
        "output_tokens": output,
        "output_tokens_details": {"reasoning_tokens": reasoning},
        "total_tokens": input + output,
    })
}

#[test]
fn parses_delta_tool_citation_completed() {
    let text = "Rust is fast.";
    let events = feed(&[
        (
            "response.created",
            json!({"type": "response.created", "response": {"id": "resp_abc", "status": "in_progress"}}),
        ),
        (
            "response.web_search_call.searching",
            json!({"type": "response.web_search_call.searching", "item_id": "ws_1", "output_index": 0}),
        ),
        (
            "response.web_search_call.completed",
            json!({"type": "response.web_search_call.completed", "item_id": "ws_1", "output_index": 0}),
        ),
        (
            "response.output_text.delta",
            json!({"type": "response.output_text.delta", "item_id": "msg_1", "output_index": 1, "content_index": 0, "delta": text}),
        ),
        (
            "response.output_text.annotation.added",
            json!({
                "type": "response.output_text.annotation.added",
                "item_id": "msg_1", "output_index": 1, "content_index": 0, "annotation_index": 0,
                "annotation": {"type": "url_citation", "url": "https://rust-lang.org", "title": "Rust", "start_index": 0, "end_index": 4},
            }),
        ),
        (
            "response.completed",
            json!({
                "type": "response.completed",
                "response": {
                    "id": "resp_abc",
                    "status": "completed",
                    "output": [{
                        "type": "message", "id": "msg_1", "role": "assistant",
                        "content": [{"type": "output_text", "text": text, "annotations": [
                            {"type": "url_citation", "url": "https://rust-lang.org", "title": "Rust", "start_index": 0, "end_index": 4}
                        ]}],
                    }],
                    "usage": usage(100, 20, 30, 5),
                },
            }),
        ),
    ]);

    assert_eq!(
        events,
        vec![
            LlmEvent::ToolStart {
                name: "web_search".to_owned(),
                details: json!({}),
            },
            LlmEvent::ToolDone {
                name: "web_search".to_owned(),
                details: json!({}),
            },
            LlmEvent::TextDelta(text.to_owned()),
            LlmEvent::Citation(RawCitation::Web {
                url: "https://rust-lang.org".to_owned(),
                title: "Rust".to_owned(),
                snippet: "Rust".to_owned(),
                span: Some((0, 4)),
            }),
            LlmEvent::Completed {
                usage: Some(LlmUsage {
                    input_tokens: 100,
                    output_tokens: 20,
                    cache_read_input_tokens: 30,
                    cache_write_input_tokens: 0,
                    reasoning_tokens: 5,
                }),
                response_id: Some("resp_abc".to_owned()),
                incomplete_reason: None,
            },
        ]
    );
}

#[test]
fn search_in_progress_events_emit_nothing() {
    let events = feed(&[
        (
            "response.web_search_call.in_progress",
            json!({"type": "response.web_search_call.in_progress", "item_id": "ws_1"}),
        ),
        (
            "response.file_search_call.in_progress",
            json!({"type": "response.file_search_call.in_progress", "item_id": "fs_1"}),
        ),
    ]);
    assert!(events.is_empty(), "{events:?}");
}

#[test]
fn each_searching_event_is_a_tool_start() {
    let searching = (
        "response.web_search_call.searching",
        json!({"type": "response.web_search_call.searching", "item_id": "ws_1"}),
    );
    let events = feed(&[searching.clone(), searching.clone(), searching]);
    let start = LlmEvent::ToolStart {
        name: "web_search".to_owned(),
        details: json!({}),
    };
    assert_eq!(events, vec![start.clone(), start.clone(), start]);
}

#[test]
fn final_annotations_add_file_citations_and_dedupe() {
    let ann =
        json!({"type": "file_citation", "file_id": "file-abc", "filename": "a.pdf", "index": 3});
    let events = feed(&[
        (
            "response.output_text.annotation.added",
            json!({"type": "response.output_text.annotation.added", "item_id": "m", "content_index": 0, "annotation": ann}),
        ),
        (
            "response.completed",
            json!({"type": "response.completed", "response": {"id": "resp_1", "output": [
                {"type": "message", "id": "m", "content": [{"type": "output_text", "text": "x", "annotations": [
                    ann,
                    {"type": "url_citation", "url": "https://a.b", "title": "AB", "start_index": 50, "end_index": 60},
                ]}]}
            ], "usage": usage(1, 1, 0, 0)}}),
        ),
    ]);
    assert_eq!(
        events[0],
        LlmEvent::Citation(RawCitation::File {
            provider_file_id: "file-abc".to_owned(),
            span: None,
        })
    );
    // Range outside the part text → empty snippet.
    assert_eq!(
        events[1],
        LlmEvent::Citation(RawCitation::Web {
            url: "https://a.b".to_owned(),
            title: "AB".to_owned(),
            snippet: String::new(),
            span: Some((50, 60)),
        })
    );
    assert!(matches!(events[2], LlmEvent::Completed { .. }));
    assert_eq!(events.len(), 3);
}

#[test]
fn event_name_falls_back_to_type_field() {
    let mut st = ParseState::default();
    let data = json!({"type": "response.output_text.delta", "item_id": "m", "content_index": 0, "delta": "Hi"}).to_string();
    assert_eq!(
        OpenAiResponsesAdapter.parse_event(&mut st, "message", &data),
        vec![LlmEvent::TextDelta("Hi".to_owned())]
    );
    assert_eq!(
        OpenAiResponsesAdapter.parse_event(&mut st, "", &data),
        vec![LlmEvent::TextDelta("Hi".to_owned())]
    );
}

#[test]
fn file_search_done_counts_results() {
    let events = feed(&[
        (
            "response.file_search_call.searching",
            json!({"type": "response.file_search_call.searching", "item_id": "fs_1"}),
        ),
        (
            "response.file_search_call.completed",
            json!({"type": "response.file_search_call.completed", "item_id": "fs_1", "results": [{}, {}, {}]}),
        ),
        (
            "response.file_search_call.completed",
            json!({"type": "response.file_search_call.completed", "item_id": "fs_2"}),
        ),
    ]);
    assert_eq!(
        events,
        vec![
            LlmEvent::ToolStart {
                name: "file_search".to_owned(),
                details: json!({}),
            },
            LlmEvent::ToolDone {
                name: "file_search".to_owned(),
                details: json!({"files_searched": 3}),
            },
            LlmEvent::ToolDone {
                name: "file_search".to_owned(),
                details: json!({"files_searched": 0}),
            },
        ]
    );
}

#[test]
fn code_interpreter_output_joined_and_truncated() {
    let events = feed(&[
        (
            "response.code_interpreter_call.in_progress",
            json!({"type": "response.code_interpreter_call.in_progress", "item_id": "ci_1"}),
        ),
        (
            "response.code_interpreter_call.interpreting",
            json!({"type": "response.code_interpreter_call.interpreting", "item_id": "ci_1"}),
        ),
        (
            "response.output_item.done",
            json!({"type": "response.output_item.done", "item": {"type": "code_interpreter_call", "id": "ci_1",
                "outputs": [{"type": "logs", "logs": "a"}, {"type": "image", "url": "x"}, {"type": "logs", "logs": "b"}]}}),
        ),
        (
            "response.output_item.done",
            json!({"type": "response.output_item.done", "item": {"type": "code_interpreter_call", "id": "ci_2",
                "outputs": [{"type": "logs", "logs": "\u{e9}".repeat(9000)}]}}),
        ),
        (
            "response.output_item.done",
            json!({"type": "response.output_item.done", "item": {"type": "message", "id": "m"}}),
        ),
    ]);
    assert_eq!(events.len(), 3);
    assert_eq!(
        events[0],
        LlmEvent::ToolStart {
            name: "code_interpreter".to_owned(),
            details: json!({}),
        }
    );
    assert_eq!(
        events[1],
        LlmEvent::ToolDone {
            name: "code_interpreter".to_owned(),
            details: json!({"output": "a\nb"}),
        }
    );
    let LlmEvent::ToolDone { details, .. } = &events[2] else {
        panic!("expected tool done, got {:?}", events[2]);
    };
    let out = details["output"].as_str().unwrap();
    assert_eq!(out, format!("{}...[truncated]", "\u{e9}".repeat(8192)));
}

#[test]
fn incomplete_is_completed_with_reason() {
    let events = feed(&[(
        "response.incomplete",
        json!({"type": "response.incomplete", "response": {"id": "resp_9", "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"}, "usage": usage(7, 3, 0, 0)}}),
    )]);
    assert_eq!(
        events,
        vec![LlmEvent::Completed {
            usage: Some(LlmUsage {
                input_tokens: 7,
                output_tokens: 3,
                ..LlmUsage::default()
            }),
            response_id: Some("resp_9".to_owned()),
            incomplete_reason: Some("max_output_tokens".to_owned()),
        }]
    );
}

#[test]
fn failed_reads_response_error_and_usage() {
    let events = feed(&[(
        "response.failed",
        json!({"type": "response.failed", "response": {"id": "resp_f", "status": "failed",
            "error": {"code": "server_error", "message": "model crashed"}, "usage": usage(5, 2, 0, 0)}}),
    )]);
    assert_eq!(
        events,
        vec![LlmEvent::Failed {
            error: LlmError::Provider {
                message: "model crashed".to_owned(),
            },
            usage: Some(LlmUsage {
                input_tokens: 5,
                output_tokens: 2,
                ..LlmUsage::default()
            }),
        }]
    );

    // Fallback to a top-level `error`; no usage.
    let events = feed(&[(
        "response.failed",
        json!({"type": "response.failed", "response": {"id": "resp_f"}, "error": {"message": "top level"}}),
    )]);
    assert_eq!(
        events,
        vec![LlmEvent::Failed {
            error: LlmError::Provider {
                message: "top level".to_owned(),
            },
            usage: None,
        }]
    );
}

#[test]
fn error_event_flat_and_raw() {
    let mut st = ParseState::default();
    let a = OpenAiResponsesAdapter;
    let failed = |message: &str| {
        vec![LlmEvent::Failed {
            error: LlmError::Provider {
                message: message.to_owned(),
            },
            usage: None,
        }]
    };
    assert_eq!(
        a.parse_event(
            &mut st,
            "error",
            &json!({"type": "error", "code": "ERR", "message": "flat boom", "param": null})
                .to_string()
        ),
        failed("flat boom")
    );
    assert_eq!(
        a.parse_event(
            &mut st,
            "error",
            &json!({"error": {"code": "x", "message": "nested boom"}}).to_string()
        ),
        failed("nested boom")
    );
    assert_eq!(
        a.parse_event(
            &mut st,
            "error",
            &json!({"response": {"error": {"message": "response boom"}}}).to_string()
        ),
        failed("response boom")
    );
    assert_eq!(
        a.parse_event(&mut st, "error", "upstream exploded"),
        failed("upstream exploded")
    );
    // A JSON string payload is the message itself (no quotes).
    assert_eq!(
        a.parse_event(&mut st, "error", "\"string boom\""),
        failed("string boom")
    );
}

#[test]
fn unknown_and_malformed_events_are_ignored() {
    let mut st = ParseState::default();
    let a = OpenAiResponsesAdapter;
    assert!(
        a.parse_event(&mut st, "response.unknown_thing", "{}")
            .is_empty()
    );
    assert!(
        a.parse_event(&mut st, "response.output_text.delta", "not json {")
            .is_empty()
    );
    assert!(a.parse_event(&mut st, "", "garbage").is_empty());
    assert!(a.parse_event(&mut st, "", "{\"no_type\": 1}").is_empty());
    assert!(a.parse_event(&mut st, "message", "[1,2,3]").is_empty());
    assert!(
        a.parse_event(&mut st, "response.output_text.delta", "{\"delta\": 5}")
            .is_empty()
    );
    // A terminal event with an unexpected shape still terminates the stream.
    assert_eq!(
        a.parse_event(&mut st, "response.completed", "{\"response\": \"x\"}"),
        vec![LlmEvent::Completed {
            usage: None,
            response_id: None,
            incomplete_reason: None,
        }]
    );
}

#[test]
fn completion_body_parsed() {
    let body = json!({
        "id": "resp_c", "status": "completed",
        "output": [
            {"type": "reasoning", "id": "rs_1", "summary": []},
            {"type": "message", "id": "m", "content": [
                {"type": "output_text", "text": "Hello "},
                {"type": "output_text", "text": "world"},
            ]},
        ],
        "usage": usage(12, 4, 0, 1),
    });
    let r = OpenAiResponsesAdapter
        .parse_completion(body.to_string().as_bytes())
        .unwrap();
    assert_eq!(r.text, "Hello world");
    assert_eq!(
        r.usage,
        Some(LlmUsage {
            input_tokens: 12,
            output_tokens: 4,
            reasoning_tokens: 1,
            ..LlmUsage::default()
        })
    );

    let failed = json!({"id": "resp_c", "status": "failed", "error": {"message": "nope"}});
    assert_eq!(
        OpenAiResponsesAdapter.parse_completion(failed.to_string().as_bytes()),
        Err(LlmError::Provider {
            message: "nope".to_owned()
        })
    );
    assert!(matches!(
        OpenAiResponsesAdapter.parse_completion(b"<html>"),
        Err(LlmError::Provider { .. })
    ));
}

#[test]
fn function_call_item_is_a_function_call_event_without_tool_event() {
    let events = feed(&[
        (
            "response.output_item.added",
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "search_knowledge", "arguments": ""}}),
        ),
        (
            "response.function_call_arguments.delta",
            json!({"type": "response.function_call_arguments.delta", "item_id": "fc_1", "output_index": 0, "delta": "{\"query\":\"q\"}"}),
        ),
        (
            "response.output_item.done",
            json!({"type": "response.output_item.done", "output_index": 0, "item": {
                "type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "search_knowledge",
                "arguments": "{\"query\":\"q\"}", "status": "completed"}}),
        ),
        (
            "response.completed",
            json!({"type": "response.completed", "response": {"id": "resp_f", "status": "completed", "output": [], "usage": usage(9, 3, 0, 0)}}),
        ),
    ]);
    assert_eq!(
        events[0],
        LlmEvent::FunctionCall(FunctionCall {
            call_id: "call_1".to_owned(),
            name: "search_knowledge".to_owned(),
            arguments: r#"{"query":"q"}"#.to_owned(),
        })
    );
    assert!(events[1].is_terminal());
    assert_eq!(events.len(), 2);
}

#[test]
fn tool_rounds_follow_the_input_as_function_call_items() {
    let mut req = request(vec![LlmTool::SearchKnowledge]);
    req.tool_rounds = vec![knowledge_round("[\"chunk\"]")];
    let b = body(&req);
    let input = b["input"].as_array().unwrap();
    assert_eq!(input.len(), 5);
    assert_eq!(
        input[3],
        json!({"type": "function_call", "call_id": "call_1", "name": "search_knowledge", "arguments": "{\"query\":\"q\"}"})
    );
    assert_eq!(
        input[4],
        json!({"type": "function_call_output", "call_id": "call_1", "output": "[\"chunk\"]"})
    );
}
