//! Provider event parsing of the `openai_responses` adapter (DESIGN 3.3
//! "Provider Event Translation" table). One SSE event of the provider becomes
//! one internal [`ProviderEvent`]; the domain service maps those to the stable
//! SSE contract.
//!
//! Public API under test (see `S3-test-interface.md` section 2.2):
//! `mini_chat::infra::llm::openai_responses::{parse_event, ProviderEvent,
//! Completion, Failure, Annotation}`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::use_debug,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value
)]

use mini_chat::infra::llm::openai_responses::{
    Annotation, Completion, Failure, ProviderEvent, TokenBreakdown, parse_event,
};
use serde_json::{Value, json};

fn parse(name: &str, data: Value) -> ProviderEvent {
    parse_event(name, &data.to_string())
}

fn completed_with(output_text: &str, annotations: Value, usage: Value) -> Value {
    json!({
        "type": "response.completed",
        "response": {
            "id": "resp_abcdef123456",
            "status": "completed",
            "output": [{
                "type": "message", "role": "assistant", "status": "completed",
                "content": [{"type": "output_text", "text": output_text,
                             "annotations": annotations}]
            }],
            "usage": usage
        }
    })
}

// ── text ─────────────────────────────────────────────────────────────────────

#[test]
fn output_text_delta_is_a_text_delta() {
    let event = parse(
        "response.output_text.delta",
        json!({"type": "response.output_text.delta", "delta": "Hel", "item_id": "m1"}),
    );
    assert_eq!(event, ProviderEvent::TextDelta("Hel".to_owned()));
}

#[test]
fn the_event_name_falls_back_to_the_type_field_when_the_line_is_missing_or_message() {
    let data = json!({"type": "response.output_text.delta", "delta": "x"});
    for name in ["", "message"] {
        assert_eq!(
            parse(name, data.clone()),
            ProviderEvent::TextDelta("x".to_owned()),
            "event line `{name}`"
        );
    }
}

#[test]
fn the_event_line_wins_when_present() {
    let event = parse(
        "response.output_text.delta",
        json!({"delta": "named by the line"}),
    );
    assert_eq!(
        event,
        ProviderEvent::TextDelta("named by the line".to_owned())
    );
}

#[test]
fn lifecycle_noise_is_ignored() {
    for name in [
        "response.created",
        "response.in_progress",
        "response.output_text.done",
        "response.content_part.added",
        "response.content_part.done",
        "response.output_item.added",
        "response.reasoning_summary_text.delta",
        "some.unknown.event",
    ] {
        let event = parse(name, json!({"type": name, "sequence_number": 3}));
        assert_eq!(event, ProviderEvent::Ignored, "{name}");
    }
}

// ── tools ────────────────────────────────────────────────────────────────────

#[test]
fn file_search_searching_starts_the_file_search_tool() {
    let event = parse(
        "response.file_search_call.searching",
        json!({"type": "response.file_search_call.searching", "item_id": "fs_1"}),
    );
    assert_eq!(
        event,
        ProviderEvent::ToolStarted {
            name: "file_search".to_owned()
        }
    );
}

#[test]
fn file_search_completed_reports_the_number_of_results_it_carries() {
    let none = parse(
        "response.file_search_call.completed",
        json!({"type": "response.file_search_call.completed", "item_id": "fs_1"}),
    );
    assert_eq!(
        none,
        ProviderEvent::ToolDone {
            name: "file_search".to_owned(),
            details: json!({"files_searched": 0})
        },
        "the real API sends no results, the count is 0"
    );
    let two = parse(
        "response.file_search_call.completed",
        json!({"type": "response.file_search_call.completed",
               "results": [{"file_id": "a"}, {"file_id": "b"}]}),
    );
    assert_eq!(
        two,
        ProviderEvent::ToolDone {
            name: "file_search".to_owned(),
            details: json!({"files_searched": 2})
        }
    );
}

#[test]
fn web_search_searching_and_completed() {
    assert_eq!(
        parse(
            "response.web_search_call.searching",
            json!({"type": "response.web_search_call.searching"})
        ),
        ProviderEvent::ToolStarted {
            name: "web_search".to_owned()
        }
    );
    assert_eq!(
        parse(
            "response.web_search_call.completed",
            json!({"type": "response.web_search_call.completed"})
        ),
        ProviderEvent::ToolDone {
            name: "web_search".to_owned(),
            details: json!({})
        }
    );
}

#[test]
fn in_progress_tool_events_of_file_and_web_search_are_ignored() {
    for name in [
        "response.file_search_call.in_progress",
        "response.web_search_call.in_progress",
    ] {
        assert_eq!(
            parse(name, json!({"type": name})),
            ProviderEvent::Ignored,
            "{name}"
        );
    }
}

#[test]
fn code_interpreter_in_progress_starts_the_tool_and_the_other_phases_are_ignored() {
    assert_eq!(
        parse(
            "response.code_interpreter_call.in_progress",
            json!({"type": "response.code_interpreter_call.in_progress"})
        ),
        ProviderEvent::ToolStarted {
            name: "code_interpreter".to_owned()
        }
    );
    for name in [
        "response.code_interpreter_call.interpreting",
        "response.code_interpreter_call.completed",
        "response.code_interpreter_call_code.delta",
    ] {
        assert_eq!(
            parse(name, json!({"type": name})),
            ProviderEvent::Ignored,
            "{name}"
        );
    }
}

fn output_item_done(item: Value) -> Value {
    json!({"type": "response.output_item.done", "output_index": 0, "item": item})
}

#[test]
fn code_interpreter_output_item_done_carries_the_joined_log_outputs() {
    let event = parse(
        "response.output_item.done",
        output_item_done(json!({
            "type": "code_interpreter_call", "id": "ci_1", "status": "completed",
            "outputs": [{"type": "logs", "logs": "line one"},
                        {"type": "image", "url": "https://x/y.png"},
                        {"type": "logs", "logs": "line two"}]
        })),
    );
    assert_eq!(
        event,
        ProviderEvent::ToolDone {
            name: "code_interpreter".to_owned(),
            details: json!({"output": "line one\nline two"})
        }
    );
}

#[test]
fn code_interpreter_output_is_capped_at_8192_characters_with_a_suffix() {
    let exact = "a".repeat(8192);
    let event = parse(
        "response.output_item.done",
        output_item_done(json!({"type": "code_interpreter_call",
                                "outputs": [{"type": "logs", "logs": exact}]})),
    );
    assert_eq!(
        event,
        ProviderEvent::ToolDone {
            name: "code_interpreter".to_owned(),
            details: json!({"output": exact})
        },
        "exactly 8192 characters are not truncated"
    );

    let long = "b".repeat(20_000);
    let event = parse(
        "response.output_item.done",
        output_item_done(json!({"type": "code_interpreter_call",
                                "outputs": [{"type": "logs", "logs": long}]})),
    );
    let ProviderEvent::ToolDone { name, details } = event else {
        panic!("expected a tool done event");
    };
    assert_eq!(name, "code_interpreter");
    let output = details["output"].as_str().expect("output string");
    assert!(
        output.ends_with("...[truncated]"),
        "suffix: ...{}",
        &output[output.len() - 20..]
    );
    assert!(
        output.chars().count() <= 8192 + "...[truncated]".len(),
        "capped, got {} characters",
        output.chars().count()
    );
    assert!(
        output.starts_with(&"b".repeat(8000)),
        "keeps the head of the logs"
    );
}

#[test]
fn other_output_items_are_ignored() {
    for item in [
        json!({"type": "message", "role": "assistant", "content": []}),
        json!({"type": "web_search_call", "id": "ws_1"}),
        json!({"type": "file_search_call", "id": "fs_1"}),
        json!({"type": "reasoning", "id": "rs_1"}),
    ] {
        assert_eq!(
            parse("response.output_item.done", output_item_done(item.clone())),
            ProviderEvent::Ignored,
            "{item}"
        );
    }
}

// ── terminal events ──────────────────────────────────────────────────────────

#[test]
fn completed_carries_usage_and_the_response_id() {
    let event = parse(
        "response.completed",
        completed_with(
            "Hello",
            json!([]),
            json!({"input_tokens": 42, "output_tokens": 7}),
        ),
    );
    assert_eq!(
        event,
        ProviderEvent::Completed(Completion {
            response_id: Some("resp_abcdef123456".to_owned()),
            incomplete: false,
            input_tokens: 42,
            output_tokens: 7,
            breakdown: TokenBreakdown::default(),
            annotations: vec![],
        })
    );
}

#[test]
fn incomplete_is_a_completion_with_the_incomplete_flag() {
    let mut data = completed_with(
        "Hel",
        json!([]),
        json!({"input_tokens": 9, "output_tokens": 2}),
    );
    data["type"] = json!("response.incomplete");
    data["response"]["status"] = json!("incomplete");
    data["response"]["incomplete_details"] = json!({"reason": "max_output_tokens"});
    let ProviderEvent::Completed(c) = parse("response.incomplete", data) else {
        panic!("a truncated but valid completion is a Completed event");
    };
    assert!(c.incomplete);
    assert_eq!((c.input_tokens, c.output_tokens), (9, 2));
}

#[test]
fn a_completion_without_usage_or_id_has_zero_tokens_and_no_response_id() {
    let event = parse(
        "response.completed",
        json!({"type": "response.completed", "response": {"status": "completed", "output": []}}),
    );
    assert_eq!(
        event,
        ProviderEvent::Completed(Completion {
            response_id: None,
            incomplete: false,
            input_tokens: 0,
            output_tokens: 0,
            breakdown: TokenBreakdown::default(),
            annotations: vec![],
        })
    );
}

#[test]
fn completed_carries_the_cache_and_reasoning_breakdown() {
    let event = parse(
        "response.completed",
        completed_with(
            "Hello",
            json!([]),
            json!({
                "input_tokens": 500,
                "input_tokens_details": {"cached_tokens": 300},
                "output_tokens": 40,
                "output_tokens_details": {"reasoning_tokens": 7},
                "total_tokens": 540,
            }),
        ),
    );
    let ProviderEvent::Completed(c) = event else {
        panic!("a completed response is a Completed event");
    };
    assert_eq!((c.input_tokens, c.output_tokens), (500, 40));
    assert_eq!(
        c.breakdown,
        TokenBreakdown {
            cache_read: 300,
            cache_write: 0,
            reasoning: 7,
        }
    );
}

#[test]
fn incomplete_carries_the_breakdown_too() {
    let mut data = completed_with(
        "Hel",
        json!([]),
        json!({"input_tokens": 9, "input_tokens_details": {"cached_tokens": 4},
               "output_tokens": 2, "output_tokens_details": {"reasoning_tokens": 1}}),
    );
    data["type"] = json!("response.incomplete");
    data["response"]["status"] = json!("incomplete");
    let ProviderEvent::Completed(c) = parse("response.incomplete", data) else {
        panic!("a truncated but valid completion is a Completed event");
    };
    assert!(c.incomplete);
    assert_eq!((c.breakdown.cache_read, c.breakdown.reasoning), (4, 1));
}

#[test]
fn a_usage_without_details_has_an_empty_breakdown() {
    let ProviderEvent::Completed(c) = parse(
        "response.completed",
        completed_with(
            "x",
            json!([]),
            json!({"input_tokens": 1, "output_tokens": 1}),
        ),
    ) else {
        panic!("Completed expected");
    };
    assert_eq!(c.breakdown, TokenBreakdown::default());
}

#[test]
fn response_failed_keeps_the_breakdown_of_its_usage() {
    let event = parse(
        "response.failed",
        json!({"type": "response.failed", "response": {
            "id": "resp_failed0000002", "status": "failed",
            "error": {"code": "server_error", "message": "boom"},
            "usage": {"input_tokens": 30, "input_tokens_details": {"cached_tokens": 12},
                      "output_tokens": 4, "output_tokens_details": {"reasoning_tokens": 3}}}}),
    );
    let ProviderEvent::Failed(f) = event else {
        panic!("Failed expected");
    };
    assert_eq!((f.input_tokens, f.output_tokens), (30, 4));
    assert_eq!((f.breakdown.cache_read, f.breakdown.reasoning), (12, 3));
}

// ── annotations ──────────────────────────────────────────────────────────────

#[test]
fn web_annotations_carry_url_title_span_and_the_answer_text_in_the_range() {
    let text = "Rust 1.80 was released in July 2024.";
    let event = parse(
        "response.completed",
        completed_with(
            text,
            json!([{"type": "url_citation", "url": "https://example.com/rust",
                    "title": "Rust blog", "start_index": 0, "end_index": 8}]),
            json!({"input_tokens": 1, "output_tokens": 1}),
        ),
    );
    let ProviderEvent::Completed(c) = event else {
        panic!("completed");
    };
    assert_eq!(
        c.annotations,
        vec![Annotation::Web {
            url: "https://example.com/rust".to_owned(),
            title: "Rust blog".to_owned(),
            snippet: "Rust 1.8".to_owned(),
            span: Some((0, 8)),
        }]
    );
}

#[test]
fn the_annotation_range_counts_characters_not_bytes() {
    let text = "caf\u{e9} \u{4e2d}\u{6587} done";
    // characters 5..7 are the two CJK characters
    let event = parse(
        "response.completed",
        completed_with(
            text,
            json!([{"type": "url_citation", "url": "https://e.example/", "title": "t",
                    "start_index": 5, "end_index": 7}]),
            json!({}),
        ),
    );
    let ProviderEvent::Completed(c) = event else {
        panic!("completed");
    };
    let Annotation::Web { snippet, span, .. } = &c.annotations[0] else {
        panic!("web annotation");
    };
    assert_eq!(snippet, "\u{4e2d}\u{6587}");
    assert_eq!(*span, Some((5, 7)));
}

#[test]
fn an_annotation_text_wins_over_the_range_and_a_range_outside_the_text_gives_an_empty_snippet() {
    let with_text = parse(
        "response.completed",
        completed_with(
            "short",
            json!([{"type": "url_citation", "url": "https://e.example/", "title": "t",
                    "text": "quoted passage", "start_index": 0, "end_index": 3}]),
            json!({}),
        ),
    );
    let ProviderEvent::Completed(c) = with_text else {
        panic!("completed");
    };
    let Annotation::Web { snippet, .. } = &c.annotations[0] else {
        panic!("web annotation");
    };
    assert_eq!(snippet, "quoted passage");

    let outside = parse(
        "response.completed",
        completed_with(
            "short",
            json!([{"type": "url_citation", "url": "https://e.example/", "title": "t",
                    "start_index": 100, "end_index": 120}]),
            json!({}),
        ),
    );
    let ProviderEvent::Completed(c) = outside else {
        panic!("completed");
    };
    let Annotation::Web { snippet, .. } = &c.annotations[0] else {
        panic!("web annotation");
    };
    assert_eq!(snippet, "");
}

#[test]
fn a_web_annotation_without_a_range_has_no_span() {
    let event = parse(
        "response.completed",
        completed_with(
            "text",
            json!([{"type": "url_citation", "url": "https://e.example/", "title": "t"}]),
            json!({}),
        ),
    );
    let ProviderEvent::Completed(c) = event else {
        panic!("completed");
    };
    let Annotation::Web { span, .. } = &c.annotations[0] else {
        panic!("web annotation");
    };
    assert_eq!(*span, None);
}

#[test]
fn file_annotations_carry_the_provider_file_id_and_filename() {
    let event = parse(
        "response.completed",
        completed_with(
            "see the report",
            json!([{"type": "file_citation", "file_id": "file-abcdef123456",
                    "filename": "Q3_Report.pdf", "index": 12}]),
            json!({}),
        ),
    );
    let ProviderEvent::Completed(c) = event else {
        panic!("completed");
    };
    assert_eq!(
        c.annotations,
        vec![Annotation::File {
            file_id: "file-abcdef123456".to_owned(),
            filename: "Q3_Report.pdf".to_owned(),
        }]
    );
}

#[test]
fn unknown_annotation_types_are_dropped_and_order_is_kept() {
    let event = parse(
        "response.completed",
        completed_with(
            "0123456789",
            json!([
                {"type": "file_path", "file_id": "file-abcdef123456", "index": 1},
                {"type": "url_citation", "url": "https://one.example/", "title": "one",
                 "start_index": 0, "end_index": 2},
                {"type": "file_citation", "file_id": "file-111111111111", "filename": "a.pdf", "index": 0},
                {"type": "url_citation", "url": "https://two.example/", "title": "two",
                 "start_index": 2, "end_index": 4}
            ]),
            json!({}),
        ),
    );
    let ProviderEvent::Completed(c) = event else {
        panic!("completed");
    };
    let kinds: Vec<String> = c
        .annotations
        .iter()
        .map(|a| match a {
            Annotation::Web { title, .. } => format!("web:{title}"),
            Annotation::File { filename, .. } => format!("file:{filename}"),
        })
        .collect();
    assert_eq!(kinds, vec!["web:one", "file:a.pdf", "web:two"]);
}

// ── failures ─────────────────────────────────────────────────────────────────

#[test]
fn response_failed_reads_the_error_of_the_response_and_keeps_its_usage() {
    let event = parse(
        "response.failed",
        json!({"type": "response.failed", "response": {
            "id": "resp_failed0000001", "status": "failed",
            "error": {"code": "server_error", "message": "The model crashed on resp_failed0000001"},
            "usage": {"input_tokens": 30, "output_tokens": 4}}}),
    );
    assert_eq!(
        event,
        ProviderEvent::Failed(Failure {
            code: Some("server_error".to_owned()),
            message: "The model crashed on resp_failed0000001".to_owned(),
            input_tokens: 30,
            output_tokens: 4,
            breakdown: TokenBreakdown::default(),
        }),
        "the message is the raw provider text; sanitizing happens at the client boundary"
    );
}

#[test]
fn response_failed_falls_back_to_a_top_level_error() {
    let event = parse(
        "response.failed",
        json!({"type": "response.failed", "error": {"code": "x", "message": "top level"}}),
    );
    let ProviderEvent::Failed(f) = event else {
        panic!("failed");
    };
    assert_eq!(f.message, "top level");
    assert_eq!(f.code.as_deref(), Some("x"));
    assert_eq!((f.input_tokens, f.output_tokens), (0, 0));
}

#[test]
fn the_error_event_is_parsed_like_response_failed_then_as_flat_code_and_message() {
    let nested = parse(
        "error",
        json!({"type": "error", "error": {"code": "rate_limit", "message": "nested message"}}),
    );
    let ProviderEvent::Failed(f) = nested else {
        panic!("failed");
    };
    assert_eq!(f.message, "nested message");

    let flat = parse(
        "error",
        json!({"type": "error", "code": "invalid_request", "message": "flat message"}),
    );
    let ProviderEvent::Failed(f) = flat else {
        panic!("failed");
    };
    assert_eq!(f.message, "flat message");
    assert_eq!(f.code.as_deref(), Some("invalid_request"));
}

#[test]
fn unparseable_error_data_becomes_the_message() {
    let ProviderEvent::Failed(f) = parse_event("error", "upstream said: boom (not json)") else {
        panic!("failed");
    };
    assert_eq!(f.message, "upstream said: boom (not json)");
    assert_eq!(f.code, None);
}
