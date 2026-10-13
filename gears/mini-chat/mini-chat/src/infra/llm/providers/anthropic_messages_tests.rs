#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::redundant_clone, clippy::non_ascii_literal)]
//! Slice 7: the `anthropic_messages` adapter: the golden request body (case
//! `inst-case-anthropic`), the continuation of the knowledge-search loop and
//! the translation of scripted Messages streams (`inst-anthropic`), including
//! the normalization of the input usage of DESIGN (the cache counts are
//! subsets of `input_tokens`, Anthropic reports them separately).

use serde_json::{Value, json};
use uuid::Uuid;

use super::*;
use crate::domain::service::stream::context::CurrentMessage;
use crate::domain::service::stream::inputs::ImageAttachment;
use crate::domain::service::stream::tools::ToolSpec;
use crate::domain::service::stream::types::{Annotation, ToolName};
use crate::infra::llm::providers::test_fixtures::{
    Parts, all_tools, call, code_interpreter, file_search, header, keys, named, outputs,
    plan_with_history, raw, request_with, run_stream, unnamed, usage, user_hex, web_search,
};
use crate::test_doubles::LogCapture;

fn parts() -> Parts {
    Parts::new(
        ProviderKind::AnthropicMessages,
        "api.anthropic.test",
        "/v1/messages",
    )
}

fn adapter() -> AnthropicMessagesAdapter {
    AnthropicMessagesAdapter::new()
}

fn build(parts: &Parts) -> BuiltRequest {
    parts.build_with(&adapter())
}

fn body(parts: &Parts) -> Value {
    build(parts).body
}

fn image(n: u128, secondary: Option<&str>) -> ImageAttachment {
    ImageAttachment {
        id: Uuid::from_u128(n),
        provider_file_id: Some(format!("file-{n}")),
        secondary_file_id: secondary.map(str::to_owned),
    }
}

fn with_images(parts: &mut Parts, images: Vec<ImageAttachment>) {
    parts.plan.current = Some(CurrentMessage {
        content: "look".to_owned(),
        images,
    });
}

// ==== the request ================================================================================

#[test]
fn the_minimal_request_is_exactly_this_body() {
    assert_eq!(
        body(&parts()),
        json!({
            "model": "model-test",
            "system": "SYS",
            "messages": [ { "role": "user", "content": "hello" } ],
            "max_tokens": 500,
            "stream": true,
            "metadata": { "user_id": user_hex() }
        })
    );
}

#[test]
fn the_adapter_serves_the_anthropic_kind() {
    assert_eq!(adapter().kind(), ProviderKind::AnthropicMessages);
    assert_eq!(build(&parts()).kind, ProviderKind::AnthropicMessages);
}

#[test]
fn the_version_constant_is_the_dated_messages_api_version() {
    assert_eq!(ANTHROPIC_VERSION, "2023-06-01");
}

#[test]
fn the_headers_are_content_type_accept_and_the_version_and_never_a_credential() {
    let built = build(&parts());
    assert_eq!(built.uri, "/api.anthropic.test/v1/messages");
    let mut headers: Vec<_> = built
        .headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
        .collect();
    headers.sort();
    assert_eq!(
        headers,
        vec![
            ("accept".to_owned(), "text/event-stream".to_owned()),
            ("anthropic-version".to_owned(), ANTHROPIC_VERSION.to_owned()),
            ("content-type".to_owned(), "application/json".to_owned()),
        ],
        "no x-api-key, no beta header without an image block or the code execution tool"
    );
}

#[test]
fn the_model_placeholder_and_the_query_of_the_api_path_are_kept() {
    let mut parts = parts();
    parts.target.api_path = "/v1/{model}/messages?x=1".to_owned();
    assert_eq!(
        build(&parts).uri,
        "/api.anthropic.test/v1/model-test/messages?x=1"
    );
}

#[test]
fn system_is_the_instructions_and_is_omitted_when_empty() {
    let mut parts = parts();
    parts.plan.instructions = "SYS\n\nKS-GUARD".to_owned();
    assert_eq!(body(&parts)["system"], "SYS\n\nKS-GUARD");
    parts.plan.instructions = String::new();
    assert!(body(&parts).get("system").is_none());
}

#[test]
fn the_messages_are_the_summary_the_history_and_the_current_message_with_string_content() {
    let mut parts = parts();
    parts.plan = plan_with_history();
    assert_eq!(
        body(&parts)["messages"],
        json!([
            { "role": "user", "content": "PREAMBLE\n\nthe summary" },
            { "role": "user", "content": "q1" },
            { "role": "assistant", "content": "a1" },
            { "role": "user", "content": "hello" }
        ])
    );
}

#[test]
fn the_output_limit_is_max_tokens_and_the_user_is_the_metadata_user_id() {
    let mut parts = parts();
    parts.max_output = 1234;
    let body = body(&parts);
    assert_eq!(body["max_tokens"], 1234);
    assert_eq!(body["stream"], true);
    assert_eq!(body["metadata"], json!({ "user_id": user_hex() }));
    assert!(
        body.get("user").is_none(),
        "Anthropic has no top-level user"
    );
    assert!(body.get("max_completion_tokens").is_none());
    assert!(body.get("max_output_tokens").is_none());
}

// ---- images -------------------------------------------------------------------------------------

#[test]
fn each_image_is_an_image_block_with_its_secondary_file_id_after_the_text() {
    let mut parts = parts();
    with_images(
        &mut parts,
        vec![image(1, Some("sec-1")), image(2, Some("sec-2"))],
    );
    assert_eq!(
        body(&parts)["messages"][0],
        json!({
            "role": "user",
            "content": [
                { "type": "text", "text": "look" },
                { "type": "image", "source": { "type": "file", "file_id": "sec-1" } },
                { "type": "image", "source": { "type": "file", "file_id": "sec-2" } }
            ]
        })
    );
}

#[test]
fn the_primary_provider_file_id_is_never_sent_to_anthropic() {
    let mut parts = parts();
    with_images(&mut parts, vec![image(1, Some("sec-1"))]);
    let text = body(&parts).to_string();
    assert!(!text.contains("file-1"), "{text}");
}

#[test]
fn an_image_without_a_secondary_copy_is_dropped_with_a_warning() {
    let logs = LogCapture::start();
    let mut parts = parts();
    with_images(&mut parts, vec![image(1, None), image(2, Some("sec-2"))]);
    assert_eq!(
        body(&parts)["messages"][0]["content"],
        json!([
            { "type": "text", "text": "look" },
            { "type": "image", "source": { "type": "file", "file_id": "sec-2" } }
        ])
    );
    assert_eq!(logs.at(tracing::Level::WARN).len(), 1);
}

#[test]
fn when_every_image_is_dropped_the_message_is_a_plain_string_without_a_beta_header() {
    let mut parts = parts();
    with_images(&mut parts, vec![image(1, None)]);
    let built = build(&parts);
    assert_eq!(
        built.body["messages"][0],
        json!({ "role": "user", "content": "look" })
    );
    assert!(header(&built, "anthropic-beta").is_none());
}

#[test]
fn the_files_api_beta_header_is_sent_only_with_an_image_block() {
    let mut parts = parts();
    assert!(header(&build(&parts), "anthropic-beta").is_none());
    with_images(&mut parts, vec![image(1, Some("sec-1"))]);
    assert_eq!(
        header(&build(&parts), "anthropic-beta"),
        Some("files-api-2025-04-14")
    );
}

// ---- tools --------------------------------------------------------------------------------------

#[test]
fn web_search_and_code_interpreter_map_to_the_server_tools_and_file_search_is_dropped() {
    let mut parts = parts();
    parts.set_tools(all_tools(), "file_search+web_search+code_interpreter");
    let body = body(&parts);
    let tools = body["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 3, "file_search is dropped: {tools:?}");
    assert_eq!(
        tools[0],
        json!({ "type": "web_search_20250305", "name": "web_search" })
    );
    assert_eq!(
        tools[1],
        json!({ "type": "code_execution_20250522", "name": "code_execution" })
    );
    assert_eq!(tools[2]["name"], "search_knowledge");
}

#[test]
fn the_search_knowledge_tool_is_a_custom_tool_with_an_input_schema() {
    let mut parts = parts();
    parts.set_tools(vec![ToolSpec::SearchKnowledge], "none");
    let body = body(&parts);
    let tool = &body["tools"][0];
    assert_eq!(tool["name"], "search_knowledge");
    assert!(tool["description"].as_str().is_some_and(|d| !d.is_empty()));
    assert_eq!(
        tool["input_schema"],
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "What to search for." },
                "top_k": { "type": "integer", "description": "How many passages to return." }
            },
            "required": ["query"]
        })
    );
    assert!(tool.get("type").is_none(), "a custom tool has no type");
    assert!(tool.get("parameters").is_none());
}

#[test]
fn tools_are_absent_when_only_file_search_was_decided() {
    let mut parts = parts();
    parts.set_tools(vec![file_search()], "file_search");
    assert!(body(&parts).get("tools").is_none());
}

#[test]
fn the_tools_keep_the_order_of_the_decision() {
    let mut parts = parts();
    parts.set_tools(vec![ToolSpec::SearchKnowledge, web_search()], "web_search");
    let body = body(&parts);
    assert_eq!(body["tools"][0]["name"], "search_knowledge");
    assert_eq!(body["tools"][1]["name"], "web_search");
}

#[test]
fn the_code_execution_tool_needs_its_beta_header() {
    let mut parts = parts();
    parts.set_tools(vec![code_interpreter()], "code_interpreter");
    assert_eq!(
        header(&build(&parts), "anthropic-beta"),
        Some("code-execution-2025-05-22")
    );
    with_images(&mut parts, vec![image(1, Some("sec-1"))]);
    assert_eq!(
        header(&build(&parts), "anthropic-beta"),
        Some("files-api-2025-04-14,code-execution-2025-05-22"),
        "one header, files API first"
    );
}

#[test]
fn the_code_interpreter_container_files_are_not_sent() {
    let mut parts = parts();
    parts.set_tools(vec![code_interpreter()], "code_interpreter");
    assert!(!body(&parts).to_string().contains("file-sheet1"));
}

// ---- parameters ---------------------------------------------------------------------------------

#[test]
fn stop_sequences_are_sent_only_when_not_empty() {
    let mut parts = parts();
    assert!(body(&parts).get("stop_sequences").is_none());
    parts.entry.general_config.api_params.stop = vec!["END".to_owned()];
    assert_eq!(body(&parts)["stop_sequences"], json!(["END"]));
    assert!(body(&parts).get("stop").is_none());
}

#[test]
fn temperature_and_top_p_are_sent_only_when_set_and_nothing_else_is() {
    let mut parts = parts();
    let params = &mut parts.entry.general_config.api_params;
    params.temperature = Some(0.2);
    params.top_p = Some(0.9);
    params.frequency_penalty = Some(0.1);
    params.presence_penalty = Some(0.3);
    params.reasoning_effort = Some("high".to_owned());
    let body = body(&parts);
    assert_eq!(body["temperature"], 0.2);
    assert_eq!(body["top_p"], 0.9);
    for key in [
        "frequency_penalty",
        "presence_penalty",
        "reasoning_effort",
        "reasoning",
    ] {
        assert!(body.get(key).is_none(), "`{key}`");
    }
    let plain = self::body(&self::parts());
    assert!(plain.get("temperature").is_none());
    assert!(plain.get("top_p").is_none());
}

#[test]
fn extra_body_is_not_merged_for_anthropic() {
    let logs = LogCapture::start();
    let mut parts = parts();
    parts.entry.general_config.api_params.extra_body =
        Some(json!({ "top_k": 5, "seed": 1, "tools": "evil" }));
    assert_eq!(body(&parts), body(&self::parts()));
    assert!(
        logs.at(tracing::Level::WARN).is_empty(),
        "nothing is merged, nothing warns"
    );
}

#[test]
fn nothing_the_gear_must_never_send_is_in_the_body() {
    let mut parts = parts();
    parts.entry.general_config.tool_support.mcp = true;
    parts.set_tools(all_tools(), "file_search+web_search+code_interpreter");
    let text = body(&parts).to_string();
    for forbidden in [
        "previous_response_id",
        "tool_choice",
        "mcp",
        "authorization",
        "x-api-key",
        "\"store\"",
    ] {
        assert!(!text.contains(forbidden), "`{forbidden}` in {text}");
    }
}

#[test]
fn the_full_request_has_no_key_beyond_the_contract() {
    let mut parts = parts();
    let params = &mut parts.entry.general_config.api_params;
    params.temperature = Some(0.5);
    params.top_p = Some(1.0);
    params.stop = vec!["END".to_owned()];
    parts.set_tools(vec![web_search()], "web_search");
    assert_eq!(
        keys(&body(&parts)),
        [
            "max_tokens",
            "messages",
            "metadata",
            "model",
            "stop_sequences",
            "stream",
            "system",
            "temperature",
            "tools",
            "top_p"
        ]
    );
}

#[test]
fn identical_inputs_give_byte_identical_bodies() {
    let mut parts = parts();
    parts.plan = plan_with_history();
    assert_eq!(
        serde_json::to_vec(&body(&parts)).unwrap(),
        serde_json::to_vec(&body(&parts)).unwrap()
    );
}

// ==== the continuation of the knowledge-search loop ==============================================

fn previous() -> BuiltRequest {
    let mut parts = parts();
    parts.set_tools(vec![ToolSpec::SearchKnowledge], "none");
    build(&parts)
}

#[test]
fn the_continuation_appends_an_assistant_turn_with_tool_use_blocks_and_a_user_turn_with_results() {
    let previous = previous();
    let next = adapter().continue_request(&previous, &outputs()).unwrap();
    let mut expected = previous.body.clone();
    let messages = expected["messages"].as_array_mut().unwrap();
    messages.push(json!({
        "role": "assistant",
        "content": [
            { "type": "tool_use", "id": "call_1", "name": "search_knowledge",
              "input": { "query": "alpha" } },
            { "type": "tool_use", "id": "call_2", "name": "search_knowledge",
              "input": { "query": "beta" } }
        ]
    }));
    messages.push(json!({
        "role": "user",
        "content": [
            { "type": "tool_result", "tool_use_id": "call_1",
              "content": "{\"results\":[{\"text\":\"A\"}]}" },
            { "type": "tool_result", "tool_use_id": "call_2",
              "content": "{\"error\":\"search limit reached\"}" }
        ]
    }));
    assert_eq!(next.body, expected);
}

#[test]
fn the_continuation_keeps_the_tools_the_headers_and_the_target() {
    let previous = previous();
    let next = adapter().continue_request(&previous, &outputs()).unwrap();
    assert_eq!(next.kind, ProviderKind::AnthropicMessages);
    assert_eq!(next.uri, previous.uri);
    assert_eq!(next.headers, previous.headers);
    assert_eq!(next.body["tools"], previous.body["tools"]);
    assert_eq!(next.body["max_tokens"], 500);
}

#[test]
fn tool_input_that_is_not_a_json_object_becomes_an_empty_object() {
    let bad = vec![crate::domain::service::stream::ports::ToolCallOutput {
        call: call("c1", "search_knowledge", "not json"),
        output: "{}".to_owned(),
    }];
    let next = adapter().continue_request(&previous(), &bad).unwrap();
    let messages = next.body["messages"].as_array().unwrap();
    assert_eq!(
        messages[messages.len() - 2]["content"][0]["input"],
        json!({})
    );
}

#[test]
fn a_second_continuation_appends_after_the_first() {
    let first = adapter().continue_request(&previous(), &outputs()).unwrap();
    let second = adapter().continue_request(&first, &outputs()[..1]).unwrap();
    assert_eq!(second.body["messages"].as_array().unwrap().len(), 1 + 2 + 2);
}

#[test]
fn a_body_without_messages_cannot_be_continued() {
    let broken = request_with(ProviderKind::AnthropicMessages, json!({ "model": "m" }));
    assert!(matches!(
        adapter().continue_request(&broken, &outputs()),
        Err(DomainError::Internal(_))
    ));
}

// ==== the translation ============================================================================

fn start_event(input: u64, cache_read: u64, cache_write: u64, output: u64) -> SseEvent {
    named(
        "message_start",
        json!({ "type": "message_start", "message": {
            "id": "msg_01", "type": "message", "role": "assistant", "content": [],
            "usage": {
                "input_tokens": input,
                "cache_read_input_tokens": cache_read,
                "cache_creation_input_tokens": cache_write,
                "output_tokens": output
            }
        } }),
    )
}

fn block_start(index: u32, block: &Value) -> SseEvent {
    named(
        "content_block_start",
        json!({ "type": "content_block_start", "index": index, "content_block": block }),
    )
}

fn block_delta(index: u32, delta: &Value) -> SseEvent {
    named(
        "content_block_delta",
        json!({ "type": "content_block_delta", "index": index, "delta": delta }),
    )
}

fn text_chunk(index: u32, text: &str) -> SseEvent {
    block_delta(index, &json!({ "type": "text_delta", "text": text }))
}

fn block_stop(index: u32) -> SseEvent {
    named(
        "content_block_stop",
        json!({ "type": "content_block_stop", "index": index }),
    )
}

fn message_delta(stop_reason: Option<&str>, output: u64) -> SseEvent {
    named(
        "message_delta",
        json!({ "type": "message_delta",
                "delta": { "stop_reason": stop_reason, "stop_sequence": null },
                "usage": { "output_tokens": output } }),
    )
}

fn message_stop() -> SseEvent {
    named("message_stop", json!({ "type": "message_stop" }))
}

fn ping() -> SseEvent {
    named("ping", json!({ "type": "ping" }))
}

fn translate_all(events: &[SseEvent]) -> Vec<ProviderEvent> {
    run_stream(&mut *adapter().translator(), events)
}

fn text(index: u32, text: &str) -> ProviderEvent {
    ProviderEvent::TextDelta {
        text: text.to_owned(),
        item_id: None,
        content_index: Some(index),
    }
}

fn completed(usage: Option<mini_chat_sdk::UsageTokens>, reason: Option<&str>) -> ProviderEvent {
    ProviderEvent::Completed {
        usage,
        response_id: Some("msg_01".to_owned()),
        incomplete_reason: reason.map(str::to_owned),
    }
}

fn text_block() -> Value {
    json!({ "type": "text", "text": "" })
}

#[test]
fn a_text_stream_is_deltas_then_a_completion_with_the_normalized_usage() {
    let events = [
        start_event(25, 10, 5, 1),
        block_start(0, &text_block()),
        ping(),
        text_chunk(0, "Hel"),
        text_chunk(0, "lo"),
        block_stop(0),
        message_delta(Some("end_turn"), 15),
        message_stop(),
    ];
    assert_eq!(
        translate_all(&events),
        vec![
            text(0, "Hel"),
            text(0, "lo"),
            // input = 25 + 10 cache read + 5 cache creation; the cache counts keep their values.
            completed(Some(usage(40, 15, 10, 5, 0)), None),
        ]
    );
}

#[test]
fn the_input_tokens_include_the_cache_read_and_the_cache_creation_tokens() {
    for (input, read, write, expected_input) in [
        (100, 0, 0, 100),
        (100, 40, 0, 140),
        (100, 0, 60, 160),
        (3, 1000, 2000, 3003),
    ] {
        let events = [
            start_event(input, read, write, 1),
            message_delta(Some("end_turn"), 9),
            message_stop(),
        ];
        let out = translate_all(&events);
        assert_eq!(
            out,
            vec![completed(
                Some(usage(expected_input, 9, read, write, 0)),
                None
            )],
            "{input}/{read}/{write}"
        );
    }
}

#[test]
fn missing_cache_fields_of_the_message_start_are_zero() {
    let start = named(
        "message_start",
        json!({ "message": { "id": "msg_01", "usage": { "input_tokens": 12, "output_tokens": 1 } } }),
    );
    let out = translate_all(&[start, message_delta(Some("end_turn"), 4), message_stop()]);
    assert_eq!(out, vec![completed(Some(usage(12, 4, 0, 0, 0)), None)]);
}

#[test]
fn the_output_tokens_of_the_message_delta_replace_the_initial_ones() {
    let out = translate_all(&[
        start_event(10, 0, 0, 7),
        message_delta(Some("end_turn"), 42),
        message_stop(),
    ]);
    assert_eq!(out, vec![completed(Some(usage(10, 42, 0, 0, 0)), None)]);
}

#[test]
fn without_a_message_delta_the_initial_output_tokens_are_reported() {
    let out = translate_all(&[start_event(10, 0, 0, 7), message_stop()]);
    assert_eq!(out, vec![completed(Some(usage(10, 7, 0, 0, 0)), None)]);
}

#[test]
fn a_stream_without_any_usage_completes_without_usage_and_without_a_response_id() {
    let out = translate_all(&[message_stop()]);
    assert_eq!(
        out,
        vec![ProviderEvent::Completed {
            usage: None,
            response_id: None,
            incomplete_reason: None,
        }]
    );
}

#[test]
fn a_message_delta_without_a_message_start_still_reports_its_output_tokens() {
    let out = translate_all(&[message_delta(Some("end_turn"), 5), message_stop()]);
    assert_eq!(
        out,
        vec![ProviderEvent::Completed {
            usage: Some(usage(0, 5, 0, 0, 0)),
            response_id: None,
            incomplete_reason: None,
        }]
    );
}

#[test]
fn the_stop_reasons_map_to_completion_reasons() {
    for (stop, reason) in [
        (Some("end_turn"), None),
        (Some("stop_sequence"), None),
        (Some("max_tokens"), Some("max_tokens")),
        (Some("pause_turn"), None),
        (None, None),
    ] {
        let out = translate_all(&[
            start_event(1, 0, 0, 1),
            message_delta(stop, 2),
            message_stop(),
        ]);
        assert_eq!(
            out,
            vec![completed(Some(usage(1, 2, 0, 0, 0)), reason)],
            "{stop:?}"
        );
    }
}

#[test]
fn the_message_stop_is_the_only_terminal_event() {
    let out = translate_all(&[start_event(1, 0, 0, 1), message_delta(Some("end_turn"), 2)]);
    assert!(
        out.is_empty(),
        "a stream that ends before message_stop has no outcome"
    );
}

#[test]
fn a_web_search_is_a_tool_start_and_a_tool_done_with_empty_details() {
    let events = [
        start_event(5, 0, 0, 1),
        block_start(0, &text_block()),
        text_chunk(0, "Searching."),
        block_stop(0),
        block_start(
            1,
            &json!({ "type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {} }),
        ),
        block_delta(
            1,
            &json!({ "type": "input_json_delta", "partial_json": "{\"query\":\"rust\"}" }),
        ),
        block_stop(1),
        block_start(
            2,
            &json!({ "type": "web_search_tool_result", "tool_use_id": "srvtoolu_1", "content": [] }),
        ),
        block_stop(2),
        message_delta(Some("end_turn"), 3),
        message_stop(),
    ];
    assert_eq!(
        translate_all(&events),
        vec![
            text(0, "Searching."),
            ProviderEvent::ToolStart {
                name: ToolName::WebSearch,
                details: json!({}),
            },
            ProviderEvent::ToolDone {
                name: ToolName::WebSearch,
                details: json!({}),
            },
            completed(Some(usage(5, 3, 0, 0, 0)), None),
        ]
    );
}

#[test]
fn a_code_execution_is_mapped_to_the_code_interpreter_without_output() {
    let events = [
        block_start(
            0,
            &json!({ "type": "server_tool_use", "id": "srvtoolu_2", "name": "code_execution", "input": {} }),
        ),
        block_stop(0),
        block_start(
            1,
            &json!({ "type": "code_execution_tool_result", "tool_use_id": "srvtoolu_2",
                     "content": { "type": "code_execution_result", "stdout": "42\n", "stderr": "" } }),
        ),
        block_stop(1),
    ];
    assert_eq!(
        translate_all(&events),
        vec![
            ProviderEvent::ToolStart {
                name: ToolName::CodeInterpreter,
                details: json!({}),
            },
            ProviderEvent::ToolDone {
                name: ToolName::CodeInterpreter,
                details: json!({}),
            },
        ]
    );
}

#[test]
fn an_unknown_server_tool_and_its_results_are_ignored() {
    let events = [
        block_start(
            0,
            &json!({ "type": "server_tool_use", "id": "x", "name": "something_new", "input": {} }),
        ),
        block_start(
            1,
            &json!({ "type": "something_new_tool_result", "tool_use_id": "x" }),
        ),
    ];
    assert!(translate_all(&events).is_empty());
}

#[test]
fn a_web_search_citation_is_an_annotation_with_its_cited_text_as_the_snippet() {
    let events = [
        block_start(0, &text_block()),
        text_chunk(0, "Rust 1.80 was released."),
        block_delta(
            0,
            &json!({ "type": "citations_delta", "citation": {
                "type": "web_search_result_location",
                "url": "https://example.test/rust",
                "title": "Rust blog",
                "encrypted_index": "abc",
                "cited_text": "Rust 1.80 is out"
            } }),
        ),
    ];
    assert_eq!(
        translate_all(&events),
        vec![
            text(0, "Rust 1.80 was released."),
            ProviderEvent::Annotation(Annotation {
                value: json!({
                    "type": "web_search_result_location",
                    "url": "https://example.test/rust",
                    "title": "Rust blog",
                    "encrypted_index": "abc",
                    "cited_text": "Rust 1.80 is out",
                    "text": "Rust 1.80 is out"
                }),
                item_id: None,
                content_index: Some(0),
                part_text: None,
            }),
        ]
    );
}

#[test]
fn citations_of_another_type_and_other_deltas_are_ignored() {
    let events = [
        block_delta(
            0,
            &json!({ "type": "citations_delta", "citation": { "type": "char_location", "cited_text": "x" } }),
        ),
        block_delta(0, &json!({ "type": "thinking_delta", "thinking": "hmm" })),
        block_delta(0, &json!({ "type": "signature_delta", "signature": "sig" })),
    ];
    assert!(translate_all(&events).is_empty());
}

#[test]
fn a_tool_use_block_is_a_tool_start_by_name_and_the_calls_are_released_at_the_message_stop() {
    let events = [
        start_event(30, 0, 0, 1),
        block_start(0, &text_block()),
        text_chunk(0, "Let me look."),
        block_stop(0),
        block_start(
            1,
            &json!({ "type": "tool_use", "id": "toolu_1", "name": "search_knowledge", "input": {} }),
        ),
        block_delta(
            1,
            &json!({ "type": "input_json_delta", "partial_json": "" }),
        ),
        block_delta(
            1,
            &json!({ "type": "input_json_delta", "partial_json": "{\"query\": " }),
        ),
        block_delta(
            1,
            &json!({ "type": "input_json_delta", "partial_json": "\"rust\"}" }),
        ),
        block_stop(1),
        message_delta(Some("tool_use"), 12),
        message_stop(),
    ];
    assert_eq!(
        translate_all(&events),
        vec![
            text(0, "Let me look."),
            ProviderEvent::ToolStart {
                name: ToolName::SearchKnowledge,
                details: json!({}),
            },
            ProviderEvent::ToolUse {
                calls: vec![call("toolu_1", "search_knowledge", "{\"query\": \"rust\"}")],
                usage: Some(usage(30, 12, 0, 0, 0)),
            },
        ],
        "no tool done event for a function tool"
    );
}

#[test]
fn tool_use_blocks_are_named_by_the_tool_and_released_in_block_order() {
    let events = [
        block_start(
            0,
            &json!({ "type": "tool_use", "id": "t0", "name": "load_files", "input": {} }),
        ),
        block_delta(
            0,
            &json!({ "type": "input_json_delta", "partial_json": "{\"ids\":[]}" }),
        ),
        block_stop(0),
        block_start(
            1,
            &json!({ "type": "tool_use", "id": "t1", "name": "get_weather", "input": {} }),
        ),
        block_stop(1),
        message_delta(Some("tool_use"), 3),
        message_stop(),
    ];
    assert_eq!(
        translate_all(&events),
        vec![
            ProviderEvent::ToolStart {
                name: ToolName::LoadFiles,
                details: json!({}),
            },
            ProviderEvent::ToolStart {
                name: ToolName::UnknownTool,
                details: json!({}),
            },
            ProviderEvent::ToolUse {
                calls: vec![
                    call("t0", "load_files", "{\"ids\":[]}"),
                    call("t1", "get_weather", "{}")
                ],
                usage: Some(usage(0, 3, 0, 0, 0)),
            },
        ],
        "a tool use without input deltas has `{{}}` arguments"
    );
}

#[test]
fn the_input_of_a_server_tool_is_not_a_function_call() {
    let events = [
        block_start(
            0,
            &json!({ "type": "server_tool_use", "id": "s", "name": "web_search", "input": {} }),
        ),
        block_delta(
            0,
            &json!({ "type": "input_json_delta", "partial_json": "{\"query\":\"q\"}" }),
        ),
        block_stop(0),
        block_start(
            1,
            &json!({ "type": "tool_use", "id": "t", "name": "search_knowledge", "input": {} }),
        ),
        block_stop(1),
        message_delta(Some("tool_use"), 3),
        message_stop(),
    ];
    let out = translate_all(&events);
    let Some(ProviderEvent::ToolUse { calls, .. }) = out.last() else {
        panic!("a tool use expected: {out:?}");
    };
    assert_eq!(calls, &vec![call("t", "search_knowledge", "{}")]);
}

#[test]
fn a_tool_use_stop_reason_without_tool_blocks_completes_normally_with_no_calls() {
    let out = translate_all(&[message_delta(Some("tool_use"), 1), message_stop()]);
    assert!(
        matches!(
            out.as_slice(),
            [ProviderEvent::Completed {
                incomplete_reason: None,
                ..
            }]
        ),
        "{out:?}"
    );
}

#[test]
fn an_error_event_is_a_provider_error_with_the_error_type_as_its_code() {
    let event = named(
        "error",
        json!({ "type": "error", "error": { "type": "overloaded_error", "message": "Overloaded" } }),
    );
    assert_eq!(
        translate_all(&[start_event(5, 0, 0, 1), event]),
        vec![ProviderEvent::ProviderError {
            code: Some("overloaded_error".to_owned()),
            message: "Overloaded".to_owned(),
            usage: None,
        }]
    );
}

#[test]
fn an_error_event_whose_data_does_not_parse_uses_the_data_as_the_message() {
    let event = named("error", json!("plain text"));
    let out = translate_all(&[SseEvent {
        data: "oops, not json".to_owned(),
        ..event
    }]);
    assert_eq!(
        out,
        vec![ProviderEvent::ProviderError {
            code: None,
            message: "oops, not json".to_owned(),
            usage: None,
        }]
    );
}

#[test]
fn the_event_name_comes_from_the_type_when_the_line_is_missing_empty_or_message() {
    let data = json!({ "type": "content_block_delta", "index": 0,
                       "delta": { "type": "text_delta", "text": "x" } });
    for name in [None, Some(""), Some("message")] {
        let event = SseEvent {
            event: name.map(str::to_owned),
            data: data.to_string(),
            id: None,
        };
        assert_eq!(translate_all(&[event]), vec![text(0, "x")], "{name:?}");
    }
}

#[test]
fn pings_unknown_events_and_malformed_data_are_ignored() {
    let events = [
        ping(),
        named("something_new", json!({ "type": "something_new" })),
        raw("not json"),
        unnamed(json!({ "no": "type" })),
        named(
            "content_block_delta",
            json!({ "type": "content_block_delta" }),
        ),
    ];
    assert!(translate_all(&events).is_empty());
}

#[test]
fn nothing_is_pending_at_the_end_of_the_body() {
    assert!(adapter().translator().finish().is_empty());
}

#[test]
fn the_adapter_creates_independent_translators() {
    let adapter = adapter();
    let mut first = adapter.translator();
    let mut second = adapter.translator();
    first.translate(&start_event(10, 0, 0, 1));
    assert_eq!(
        second.translate(&message_stop()),
        vec![ProviderEvent::Completed {
            usage: None,
            response_id: None,
            incomplete_reason: None,
        }],
        "the second one never saw the message start"
    );
}
