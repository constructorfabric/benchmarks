//! The wire translation of the four adapter kinds against recorded-style fixtures: `OpenAI`
//! Responses (including `response.incomplete`, `response.failed` with usage, the bare `error`
//! event and the missing `event:` line), Chat Completions, `vLLM` with `<think>` blocks and
//! Anthropic with `code_execution`, web search and function tool blocks.
//!
//! Slice S2-provider-path of `cpt-cf-mini-chat-feature-message-streaming`: algorithms
//! `event-translation` and `adapter-kinds`; definition of done
//! `cpt-cf-mini-chat-dod-message-streaming-provider-adapters`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mini_chat_sdk::UsageTokens;
use serde_json::{Value, json};

use super::provider_events::{
    Annotation, OutputPart, ProviderCompletion, ProviderEvent, ToolCallData, WireEvent, WireParser,
    parser_for,
};
use crate::config::ProviderKind;

fn named(parser: &mut dyn WireParser, name: &str, data: &Value) -> Vec<ProviderEvent> {
    parser.feed(&WireEvent::named(name, &data.to_string()))
}

fn responses() -> Box<dyn WireParser> {
    parser_for(ProviderKind::OpenaiResponses)
}

fn usage(
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    reasoning: u64,
) -> UsageTokens {
    UsageTokens {
        input_tokens: input,
        output_tokens: output,
        cache_read_input_tokens: cache_read,
        cache_write_input_tokens: cache_write,
        reasoning_tokens: reasoning,
    }
}

fn completion(events: &[ProviderEvent]) -> ProviderCompletion {
    match events {
        [ProviderEvent::Completed(completion)] => completion.clone(),
        other => panic!("expected one completion, got {other:?}"),
    }
}

// ── OpenAI Responses ─────────────────────────────────────────────────────────

#[test]
fn a_text_delta_is_a_text_event() {
    let mut parser = responses();
    let events = named(
        parser.as_mut(),
        "response.output_text.delta",
        &json!({ "type": "response.output_text.delta", "delta": "Hel" }),
    );
    assert_eq!(events, vec![ProviderEvent::Text("Hel".to_owned())]);
}

#[test]
fn the_event_name_comes_from_the_type_when_the_event_line_is_missing_or_is_message() {
    let data = json!({ "type": "response.output_text.delta", "delta": "x" }).to_string();
    let mut parser = responses();
    assert_eq!(
        parser.feed(&WireEvent::data_only(&data)),
        vec![ProviderEvent::Text("x".to_owned())]
    );
    assert_eq!(
        parser.feed(&WireEvent::named("message", &data)),
        vec![ProviderEvent::Text("x".to_owned())]
    );
}

#[test]
fn file_search_start_and_done_report_the_number_of_results() {
    let mut parser = responses();
    assert_eq!(
        named(
            parser.as_mut(),
            "response.file_search_call.searching",
            &json!({ "type": "response.file_search_call.searching" })
        ),
        vec![ProviderEvent::ToolStart {
            name: "file_search".to_owned(),
            details: json!({}),
        }]
    );
    assert_eq!(
        named(
            parser.as_mut(),
            "response.file_search_call.completed",
            &json!({ "type": "response.file_search_call.completed",
                      "results": [{ "file_id": "file-a" }, { "file_id": "file-b" }] })
        ),
        vec![ProviderEvent::ToolDone {
            name: "file_search".to_owned(),
            details: json!({ "files_searched": 2 }),
        }]
    );
    // the real API sends no results
    assert_eq!(
        named(
            parser.as_mut(),
            "response.file_search_call.completed",
            &json!({ "type": "response.file_search_call.completed" })
        ),
        vec![ProviderEvent::ToolDone {
            name: "file_search".to_owned(),
            details: json!({ "files_searched": 0 }),
        }]
    );
}

#[test]
fn web_search_start_and_done_have_empty_details() {
    let mut parser = responses();
    assert_eq!(
        named(
            parser.as_mut(),
            "response.web_search_call.searching",
            &json!({ "type": "response.web_search_call.searching" })
        ),
        vec![ProviderEvent::ToolStart {
            name: "web_search".to_owned(),
            details: json!({}),
        }]
    );
    assert_eq!(
        named(
            parser.as_mut(),
            "response.web_search_call.completed",
            &json!({ "type": "response.web_search_call.completed" })
        ),
        vec![ProviderEvent::ToolDone {
            name: "web_search".to_owned(),
            details: json!({}),
        }]
    );
}

#[test]
fn code_interpreter_starts_on_in_progress_and_finishes_with_the_joined_log_output() {
    let mut parser = responses();
    assert_eq!(
        named(
            parser.as_mut(),
            "response.code_interpreter_call.in_progress",
            &json!({ "type": "response.code_interpreter_call.in_progress" })
        ),
        vec![ProviderEvent::ToolStart {
            name: "code_interpreter".to_owned(),
            details: json!({}),
        }]
    );
    for ignored in [
        "response.code_interpreter_call.interpreting",
        "response.code_interpreter_call.completed",
    ] {
        assert!(
            named(parser.as_mut(), ignored, &json!({ "type": ignored })).is_empty(),
            "{ignored}"
        );
    }
    let done = named(
        parser.as_mut(),
        "response.output_item.done",
        &json!({
            "type": "response.output_item.done",
            "item": { "type": "code_interpreter_call", "outputs": [
                { "type": "logs", "logs": "line one" },
                { "type": "image", "url": "https://example.com/x.png" },
                { "type": "logs", "logs": "line two" },
            ] }
        }),
    );
    assert_eq!(
        done,
        vec![ProviderEvent::ToolDone {
            name: "code_interpreter".to_owned(),
            details: json!({ "output": "line one\nline two" }),
        }]
    );
}

#[test]
fn a_long_code_interpreter_output_is_capped_with_the_truncation_suffix() {
    let mut parser = responses();
    let long = "a".repeat(9000);
    let events = named(
        parser.as_mut(),
        "response.output_item.done",
        &json!({
            "type": "response.output_item.done",
            "item": { "type": "code_interpreter_call",
                       "outputs": [{ "type": "logs", "logs": long }] }
        }),
    );
    let [ProviderEvent::ToolDone { details, .. }] = events.as_slice() else {
        panic!("{events:?}");
    };
    let output = details["output"].as_str().unwrap();
    assert!(output.ends_with("...[truncated]"), "{}", output.len());
    let length = output.chars().count();
    assert!(
        (8192..=8192 + "...[truncated]".len()).contains(&length),
        "{length}"
    );
    assert!(output.starts_with(&"a".repeat(8000)));
}

#[test]
fn other_output_items_are_ignored_and_a_function_call_is_a_tool_use_outcome() {
    let mut parser = responses();
    assert!(
        named(
            parser.as_mut(),
            "response.output_item.done",
            &json!({ "type": "response.output_item.done",
                      "item": { "type": "message", "content": [] } })
        )
        .is_empty()
    );
    assert_eq!(
        named(
            parser.as_mut(),
            "response.output_item.done",
            &json!({ "type": "response.output_item.done",
                      "item": { "type": "function_call", "name": "search_knowledge",
                                 "call_id": "call_1", "arguments": "{}" } })
        ),
        vec![ProviderEvent::ToolUse {
            name: "search_knowledge".to_owned(),
        }]
    );
}

#[test]
fn unknown_and_lifecycle_events_are_ignored() {
    let mut parser = responses();
    for name in [
        "response.created",
        "response.in_progress",
        "response.output_item.added",
        "response.content_part.added",
        "response.output_text.done",
        "response.some_future_event",
    ] {
        assert!(
            named(parser.as_mut(), name, &json!({ "type": name })).is_empty(),
            "{name}"
        );
    }
}

#[test]
fn completed_carries_usage_the_response_id_and_the_annotated_parts() {
    let mut parser = responses();
    let events = named(
        parser.as_mut(),
        "response.completed",
        &json!({
            "type": "response.completed",
            "response": {
                "id": "resp_0aB1c2D3e4F5g6H7",
                "status": "completed",
                "output": [{
                    "type": "message",
                    "content": [{
                        "type": "output_text",
                        "text": "See the docs for details.",
                        "annotations": [
                            { "type": "url_citation", "url": "https://example.com/docs",
                              "title": "Docs", "start_index": 4, "end_index": 12 },
                            { "type": "file_citation", "file_id": "file-AbC123", "filename": "a.pdf",
                              "index": 0 },
                        ]
                    }]
                }],
                "usage": {
                    "input_tokens": 120, "output_tokens": 30, "total_tokens": 150,
                    "input_tokens_details": { "cached_tokens": 20 },
                    "output_tokens_details": { "reasoning_tokens": 7 }
                }
            }
        }),
    );
    let completion = completion(&events);
    assert_eq!(completion.usage, Some(usage(120, 30, 20, 0, 7)));
    assert_eq!(
        completion.response_id.as_deref(),
        Some("resp_0aB1c2D3e4F5g6H7")
    );
    assert_eq!(completion.incomplete_reason, None);
    assert_eq!(
        completion.parts,
        vec![OutputPart {
            text: "See the docs for details.".to_owned(),
            annotations: vec![
                Annotation::Url {
                    url: "https://example.com/docs".to_owned(),
                    title: Some("Docs".to_owned()),
                    start_index: Some(4),
                    end_index: Some(12),
                    text: None,
                },
                Annotation::File {
                    file_id: "file-AbC123".to_owned(),
                    filename: Some("a.pdf".to_owned()),
                },
            ],
        }]
    );
}

#[test]
fn completed_without_usage_has_none() {
    let mut parser = responses();
    let events = named(
        parser.as_mut(),
        "response.completed",
        &json!({ "type": "response.completed",
                  "response": { "id": "resp_x", "status": "completed", "output": [] } }),
    );
    assert_eq!(completion(&events).usage, None);
}

#[test]
fn incomplete_is_a_truncated_completion_with_the_provider_reason_and_usage() {
    let mut parser = responses();
    let events = named(
        parser.as_mut(),
        "response.incomplete",
        &json!({
            "type": "response.incomplete",
            "response": {
                "id": "resp_inc", "status": "incomplete",
                "incomplete_details": { "reason": "max_output_tokens" },
                "output": [],
                "usage": { "input_tokens": 10, "output_tokens": 500 }
            }
        }),
    );
    let completion = completion(&events);
    assert_eq!(
        completion.incomplete_reason.as_deref(),
        Some("max_output_tokens")
    );
    assert_eq!(completion.usage, Some(usage(10, 500, 0, 0, 0)));
    assert_eq!(completion.response_id.as_deref(), Some("resp_inc"));
}

#[test]
fn incomplete_without_a_response_id_is_still_a_completion() {
    let mut parser = responses();
    let events = named(
        parser.as_mut(),
        "response.incomplete",
        &json!({ "type": "response.incomplete",
                  "response": { "incomplete_details": { "reason": "content_filter" } } }),
    );
    let completion = completion(&events);
    assert_eq!(
        completion.incomplete_reason.as_deref(),
        Some("content_filter")
    );
    assert_eq!(completion.response_id, None);
}

#[test]
fn failed_reads_the_error_of_the_response_and_keeps_its_usage() {
    let mut parser = responses();
    let events = named(
        parser.as_mut(),
        "response.failed",
        &json!({
            "type": "response.failed",
            "response": {
                "id": "resp_f", "status": "failed",
                "error": { "code": "server_error", "message": "The model crashed" },
                "usage": { "input_tokens": 80, "output_tokens": 12 }
            }
        }),
    );
    let [ProviderEvent::Failed(failure)] = events.as_slice() else {
        panic!("{events:?}");
    };
    assert_eq!(failure.code.as_deref(), Some("server_error"));
    assert_eq!(failure.message.as_deref(), Some("The model crashed"));
    assert_eq!(failure.usage, Some(usage(80, 12, 0, 0, 0)));
}

#[test]
fn failed_falls_back_to_the_top_level_error() {
    let mut parser = responses();
    let events = named(
        parser.as_mut(),
        "response.failed",
        &json!({ "type": "response.failed", "response": { "status": "failed" },
                  "error": { "code": "rate_limit_exceeded", "message": "Slow down" } }),
    );
    let [ProviderEvent::Failed(failure)] = events.as_slice() else {
        panic!("{events:?}");
    };
    assert_eq!(failure.code.as_deref(), Some("rate_limit_exceeded"));
    assert_eq!(failure.message.as_deref(), Some("Slow down"));
    assert_eq!(failure.usage, None);
}

#[test]
fn a_bare_error_event_is_parsed_nested_then_flat_then_as_plain_text() {
    let mut parser = responses();
    let nested = named(
        parser.as_mut(),
        "error",
        &json!({ "type": "error", "error": { "code": "invalid_prompt", "message": "Bad prompt" } }),
    );
    let [ProviderEvent::Failed(failure)] = nested.as_slice() else {
        panic!("{nested:?}");
    };
    assert_eq!(failure.code.as_deref(), Some("invalid_prompt"));
    assert_eq!(failure.message.as_deref(), Some("Bad prompt"));

    let flat = named(
        parser.as_mut(),
        "error",
        &json!({ "code": "overloaded", "message": "Try later" }),
    );
    let [ProviderEvent::Failed(failure)] = flat.as_slice() else {
        panic!("{flat:?}");
    };
    assert_eq!(failure.code.as_deref(), Some("overloaded"));
    assert_eq!(failure.message.as_deref(), Some("Try later"));

    let text = parser.feed(&WireEvent::named("error", "upstream exploded"));
    let [ProviderEvent::Failed(failure)] = text.as_slice() else {
        panic!("{text:?}");
    };
    assert_eq!(failure.message.as_deref(), Some("upstream exploded"));
}

// ── vLLM Responses ───────────────────────────────────────────────────────────

fn vllm_collect(deltas: &[&str]) -> (String, String) {
    let mut parser = parser_for(ProviderKind::VllmResponses);
    let (mut reasoning, mut text) = (String::new(), String::new());
    for delta in deltas {
        for event in named(
            parser.as_mut(),
            "response.output_text.delta",
            &json!({ "type": "response.output_text.delta", "delta": delta }),
        ) {
            match event {
                ProviderEvent::Reasoning(part) => reasoning.push_str(&part),
                ProviderEvent::Text(part) => text.push_str(&part),
                other => panic!("{other:?}"),
            }
        }
    }
    (reasoning, text)
}

#[test]
fn vllm_emits_think_blocks_as_reasoning_across_deltas() {
    let (reasoning, text) = vllm_collect(&["<think>", "I think", " so", "</think>", "Answer"]);
    assert_eq!(reasoning, "I think so");
    assert_eq!(text, "Answer");
}

#[test]
fn vllm_handles_the_tags_inside_one_delta() {
    let (reasoning, text) = vllm_collect(&["<think>abc</think>def"]);
    assert_eq!((reasoning.as_str(), text.as_str()), ("abc", "def"));
    let (reasoning, text) = vllm_collect(&["Hi <think>x</think>"]);
    assert_eq!((reasoning.as_str(), text.as_str()), ("x", "Hi "));
}

#[test]
fn vllm_text_without_think_blocks_is_plain_text() {
    let (reasoning, text) = vllm_collect(&["plain ", "answer"]);
    assert_eq!((reasoning.as_str(), text.as_str()), ("", "plain answer"));
}

#[test]
fn vllm_translates_the_other_events_like_the_responses_adapter() {
    let mut parser = parser_for(ProviderKind::VllmResponses);
    let events = named(
        parser.as_mut(),
        "response.completed",
        &json!({ "type": "response.completed",
                  "response": { "id": "resp_v", "output": [],
                                 "usage": { "input_tokens": 5, "output_tokens": 6 } } }),
    );
    assert_eq!(completion(&events).usage, Some(usage(5, 6, 0, 0, 0)));
}

// ── Chat Completions ─────────────────────────────────────────────────────────

fn chunk(parser: &mut dyn WireParser, data: &Value) -> Vec<ProviderEvent> {
    parser.feed(&WireEvent::data_only(&data.to_string()))
}

fn non_empty(events: Vec<ProviderEvent>) -> Vec<ProviderEvent> {
    events
        .into_iter()
        .filter(|event| !matches!(event, ProviderEvent::Text(text) if text.is_empty()))
        .collect()
}

#[test]
fn chat_completions_maps_content_deltas_and_completes_on_the_usage_chunk_and_done() {
    let mut parser = parser_for(ProviderKind::OpenaiChatCompletions);
    let first = chunk(
        parser.as_mut(),
        &json!({ "id": "chatcmpl-AbC", "choices": [{ "index": 0,
            "delta": { "role": "assistant", "content": "Hel" }, "finish_reason": null }] }),
    );
    assert_eq!(
        non_empty(first),
        vec![ProviderEvent::Text("Hel".to_owned())]
    );
    let second = chunk(
        parser.as_mut(),
        &json!({ "id": "chatcmpl-AbC", "choices": [{ "index": 0,
            "delta": { "content": "lo" }, "finish_reason": null }] }),
    );
    assert_eq!(
        non_empty(second),
        vec![ProviderEvent::Text("lo".to_owned())]
    );
    // the finish reason and the usage chunk complete nothing yet
    assert!(
        non_empty(chunk(
            parser.as_mut(),
            &json!({ "id": "chatcmpl-AbC", "choices": [{ "index": 0, "delta": {},
                "finish_reason": "stop" }] })
        ))
        .is_empty()
    );
    assert!(
        chunk(
            parser.as_mut(),
            &json!({ "id": "chatcmpl-AbC", "choices": [], "usage": {
                "prompt_tokens": 11, "completion_tokens": 7,
                "prompt_tokens_details": { "cached_tokens": 2 },
                "completion_tokens_details": { "reasoning_tokens": 3 } } })
        )
        .is_empty()
    );
    let done = completion(&parser.feed(&WireEvent::data_only("[DONE]")));
    assert_eq!(done.usage, Some(usage(11, 7, 2, 0, 3)));
    assert_eq!(done.incomplete_reason, None);
    assert!(done.parts.is_empty());
}

#[test]
fn a_length_finish_reason_is_an_incomplete_completion_with_reason_max_tokens() {
    let mut parser = parser_for(ProviderKind::OpenaiChatCompletions);
    chunk(
        parser.as_mut(),
        &json!({ "choices": [{ "index": 0, "delta": { "content": "x" }, "finish_reason": null }] }),
    );
    chunk(
        parser.as_mut(),
        &json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "length" }] }),
    );
    chunk(
        parser.as_mut(),
        &json!({ "choices": [], "usage": { "prompt_tokens": 4, "completion_tokens": 9 } }),
    );
    let done = completion(&parser.feed(&WireEvent::data_only("[DONE]")));
    assert_eq!(done.incomplete_reason.as_deref(), Some("max_tokens"));
    assert_eq!(done.usage, Some(usage(4, 9, 0, 0, 0)));
}

#[test]
fn chat_completions_tool_call_deltas_become_function_call_tool_events() {
    let mut parser = parser_for(ProviderKind::OpenaiChatCompletions);
    let start = chunk(
        parser.as_mut(),
        &json!({ "choices": [{ "index": 0, "delta": { "tool_calls": [{
            "index": 0, "id": "call_AbC123", "type": "function",
            "function": { "name": "search_knowledge", "arguments": "" } }] },
            "finish_reason": null }] }),
    );
    assert_eq!(
        start,
        vec![ProviderEvent::ToolStart {
            name: "function_call".to_owned(),
            details: json!({ "index": 0, "call_id": "call_AbC123", "name": "search_knowledge" }),
        }]
    );
    for fragment in ["{\"q\"", ":\"x\"}"] {
        assert!(
            chunk(
                parser.as_mut(),
                &json!({ "choices": [{ "index": 0, "delta": { "tool_calls": [{
                    "index": 0, "function": { "arguments": fragment } }] },
                    "finish_reason": null }] })
            )
            .is_empty(),
            "{fragment}"
        );
    }
    let finish = chunk(
        parser.as_mut(),
        &json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }] }),
    );
    assert_eq!(
        finish,
        vec![
            ProviderEvent::ToolDone {
                name: "function_call".to_owned(),
                details: json!({ "call_id": "call_AbC123", "name": "search_knowledge",
                                  "arguments": "{\"q\":\"x\"}" }),
            },
            ProviderEvent::ToolUse {
                name: "search_knowledge".to_owned(),
            },
        ]
    );
}

#[test]
fn chat_completions_tool_calls_reach_the_completion() {
    // two parallel calls (one with no arguments) and the usage chunk, then `[DONE]`
    let mut parser = parser_for(ProviderKind::OpenaiChatCompletions);
    chunk(
        parser.as_mut(),
        &json!({ "id": "chatcmpl-T", "choices": [{ "index": 0, "delta": { "tool_calls": [
            { "index": 0, "id": "call_a", "type": "function",
              "function": { "name": "search_knowledge", "arguments": "" } },
            { "index": 1, "id": "call_b", "type": "function",
              "function": { "name": "load_files", "arguments": "" } }] },
            "finish_reason": null }] }),
    );
    for fragment in ["{\"query\"", ":\"x\"}"] {
        chunk(
            parser.as_mut(),
            &json!({ "choices": [{ "index": 0, "delta": { "tool_calls": [{
                "index": 0, "function": { "arguments": fragment } }] },
                "finish_reason": null }] }),
        );
    }
    chunk(
        parser.as_mut(),
        &json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }] }),
    );
    chunk(
        parser.as_mut(),
        &json!({ "choices": [], "usage": { "prompt_tokens": 5, "completion_tokens": 3 } }),
    );
    let done = completion(&parser.feed(&WireEvent::data_only("[DONE]")));
    assert_eq!(done.usage, Some(usage(5, 3, 0, 0, 0)));
    assert_eq!(done.response_id.as_deref(), Some("chatcmpl-T"));
    assert_eq!(
        done.tool_calls,
        vec![
            ToolCallData {
                name: "search_knowledge".to_owned(),
                call_id: "call_a".to_owned(),
                arguments: "{\"query\":\"x\"}".to_owned(),
            },
            ToolCallData {
                name: "load_files".to_owned(),
                call_id: "call_b".to_owned(),
                arguments: "{}".to_owned(),
            },
        ]
    );
}

#[test]
fn a_chat_completions_text_answer_has_no_tool_calls() {
    let mut parser = parser_for(ProviderKind::OpenaiChatCompletions);
    chunk(
        parser.as_mut(),
        &json!({ "choices": [{ "index": 0, "delta": { "content": "hi" }, "finish_reason": "stop" }] }),
    );
    let done = completion(&parser.feed(&WireEvent::data_only("[DONE]")));
    assert!(done.tool_calls.is_empty());
}

#[test]
fn chat_completions_reports_a_stream_error_object() {
    let mut parser = parser_for(ProviderKind::OpenaiChatCompletions);
    let events = chunk(
        parser.as_mut(),
        &json!({ "error": { "message": "The server had an error", "type": "server_error",
                             "code": null } }),
    );
    let [ProviderEvent::Failed(failure)] = events.as_slice() else {
        panic!("{events:?}");
    };
    assert_eq!(failure.message.as_deref(), Some("The server had an error"));
}

// ── Anthropic Messages ───────────────────────────────────────────────────────

fn anthropic() -> Box<dyn WireParser> {
    parser_for(ProviderKind::AnthropicMessages)
}

fn block_start(parser: &mut dyn WireParser, index: u64, block: &Value) -> Vec<ProviderEvent> {
    named(
        parser,
        "content_block_start",
        &json!({ "type": "content_block_start", "index": index, "content_block": block }),
    )
}

fn block_stop(parser: &mut dyn WireParser, index: u64) -> Vec<ProviderEvent> {
    named(
        parser,
        "content_block_stop",
        &json!({ "type": "content_block_stop", "index": index }),
    )
}

fn message_start(parser: &mut dyn WireParser) {
    let events = named(
        parser,
        "message_start",
        &json!({ "type": "message_start", "message": { "id": "msg_01AbC", "role": "assistant",
            "usage": { "input_tokens": 25, "output_tokens": 1,
                        "cache_read_input_tokens": 3, "cache_creation_input_tokens": 4 } } }),
    );
    assert!(events.is_empty(), "{events:?}");
}

#[test]
fn anthropic_streams_text_and_completes_with_the_merged_usage_on_message_stop() {
    let mut parser = anthropic();
    message_start(parser.as_mut());
    assert!(block_start(parser.as_mut(), 0, &json!({ "type": "text", "text": "" })).is_empty());
    let delta = named(
        parser.as_mut(),
        "content_block_delta",
        &json!({ "type": "content_block_delta", "index": 0,
                  "delta": { "type": "text_delta", "text": "Hello" } }),
    );
    assert_eq!(delta, vec![ProviderEvent::Text("Hello".to_owned())]);
    assert!(block_stop(parser.as_mut(), 0).is_empty());
    assert!(
        named(
            parser.as_mut(),
            "message_delta",
            &json!({ "type": "message_delta",
                      "delta": { "stop_reason": "end_turn", "stop_sequence": null },
                      "usage": { "output_tokens": 15 } })
        )
        .is_empty()
    );
    let done = completion(&named(
        parser.as_mut(),
        "message_stop",
        &json!({ "type": "message_stop" }),
    ));
    assert_eq!(done.usage, Some(usage(25, 15, 3, 4, 0)));
    assert_eq!(done.incomplete_reason, None);
    // no citations on this adapter
    assert!(done.parts.is_empty());
}

#[test]
fn an_anthropic_max_tokens_stop_is_an_incomplete_completion() {
    let mut parser = anthropic();
    message_start(parser.as_mut());
    named(
        parser.as_mut(),
        "message_delta",
        &json!({ "type": "message_delta", "delta": { "stop_reason": "max_tokens" },
                  "usage": { "output_tokens": 500 } }),
    );
    let done = completion(&named(
        parser.as_mut(),
        "message_stop",
        &json!({ "type": "message_stop" }),
    ));
    assert_eq!(done.incomplete_reason.as_deref(), Some("max_tokens"));
}

#[test]
fn code_execution_blocks_are_code_interpreter_tool_events_with_empty_details() {
    let mut parser = anthropic();
    message_start(parser.as_mut());
    assert_eq!(
        block_start(
            parser.as_mut(),
            1,
            &json!({ "type": "server_tool_use", "id": "srvtoolu_01", "name": "code_execution",
                      "input": {} })
        ),
        vec![ProviderEvent::ToolStart {
            name: "code_interpreter".to_owned(),
            details: json!({}),
        }]
    );
    // the input JSON deltas of the tool block are not text
    assert!(
        named(
            parser.as_mut(),
            "content_block_delta",
            &json!({ "type": "content_block_delta", "index": 1,
                      "delta": { "type": "input_json_delta", "partial_json": "{\"code\":" } })
        )
        .is_empty()
    );
    assert_eq!(
        block_stop(parser.as_mut(), 1),
        vec![ProviderEvent::ToolDone {
            name: "code_interpreter".to_owned(),
            details: json!({}),
        }]
    );
}

#[test]
fn web_search_server_tool_blocks_are_web_search_tool_events_with_empty_details() {
    let mut parser = anthropic();
    message_start(parser.as_mut());
    assert_eq!(
        block_start(
            parser.as_mut(),
            2,
            &json!({ "type": "server_tool_use", "id": "srvtoolu_02", "name": "web_search",
                      "input": {} })
        ),
        vec![ProviderEvent::ToolStart {
            name: "web_search".to_owned(),
            details: json!({}),
        }]
    );
    assert_eq!(
        block_stop(parser.as_mut(), 2),
        vec![ProviderEvent::ToolDone {
            name: "web_search".to_owned(),
            details: json!({}),
        }]
    );
    // the result block of the search and its stop are not tool events and produce no citation
    assert!(
        block_start(
            parser.as_mut(),
            3,
            &json!({ "type": "web_search_tool_result", "tool_use_id": "srvtoolu_02",
                      "content": [] })
        )
        .is_empty()
    );
    assert!(block_stop(parser.as_mut(), 3).is_empty());
}

#[test]
fn a_function_tool_use_is_one_start_named_by_the_tool_and_no_done() {
    for (tool, expected) in [
        ("search_knowledge", "search_knowledge"),
        ("load_files", "load_files"),
        ("get_weather", "unknown_tool"),
    ] {
        let mut parser = anthropic();
        message_start(parser.as_mut());
        let start = block_start(
            parser.as_mut(),
            1,
            &json!({ "type": "tool_use", "id": "toolu_01AbC", "name": tool, "input": {} }),
        );
        assert_eq!(
            start,
            vec![
                ProviderEvent::ToolStart {
                    name: expected.to_owned(),
                    details: json!({}),
                },
                ProviderEvent::ToolUse {
                    name: expected.to_owned(),
                },
            ],
            "{tool}"
        );
        assert!(block_stop(parser.as_mut(), 1).is_empty(), "{tool}");
    }
}

/// The completed message carries the function tool uses with their call id and the input
/// assembled from the `input_json_delta` fragments (the knowledge search loop of
/// `cpt-cf-mini-chat-feature-attachments-and-file-search` answers them).
#[test]
fn a_function_tool_use_completes_with_its_call_id_and_assembled_input() {
    let mut parser = anthropic();
    message_start(parser.as_mut());
    block_start(
        parser.as_mut(),
        0,
        &json!({ "type": "tool_use", "id": "toolu_01AbC", "name": "search_knowledge", "input": {} }),
    );
    for fragment in ["{\"query\": \"leave", " policy\", \"top_k\": 3}"] {
        assert!(
            named(
                parser.as_mut(),
                "content_block_delta",
                &json!({ "type": "content_block_delta", "index": 0,
                          "delta": { "type": "input_json_delta", "partial_json": fragment } }),
            )
            .is_empty()
        );
    }
    assert!(block_stop(parser.as_mut(), 0).is_empty());
    // a tool use without input fragments completes with an empty object
    block_start(
        parser.as_mut(),
        1,
        &json!({ "type": "tool_use", "id": "toolu_02", "name": "load_files", "input": {} }),
    );
    assert!(block_stop(parser.as_mut(), 1).is_empty());
    let done = completion(&named(
        parser.as_mut(),
        "message_stop",
        &json!({ "type": "message_stop" }),
    ));
    assert_eq!(
        done.tool_calls,
        vec![
            ToolCallData {
                name: "search_knowledge".to_owned(),
                call_id: "toolu_01AbC".to_owned(),
                arguments: "{\"query\": \"leave policy\", \"top_k\": 3}".to_owned(),
            },
            ToolCallData {
                name: "load_files".to_owned(),
                call_id: "toolu_02".to_owned(),
                arguments: "{}".to_owned(),
            },
        ]
    );
}

#[test]
fn an_anthropic_error_event_is_a_failure_and_a_ping_is_ignored() {
    let mut parser = anthropic();
    assert!(named(parser.as_mut(), "ping", &json!({ "type": "ping" })).is_empty());
    let events = named(
        parser.as_mut(),
        "error",
        &json!({ "type": "error",
                  "error": { "type": "overloaded_error", "message": "Overloaded" } }),
    );
    let [ProviderEvent::Failed(failure)] = events.as_slice() else {
        panic!("{events:?}");
    };
    assert_eq!(failure.code.as_deref(), Some("overloaded_error"));
    assert_eq!(failure.message.as_deref(), Some("Overloaded"));
}
