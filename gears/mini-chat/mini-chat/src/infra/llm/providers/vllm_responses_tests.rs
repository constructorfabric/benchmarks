#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::redundant_clone, clippy::non_ascii_literal)]
//! Slice 7: the `vllm_responses` adapter: the golden request body (case
//! `inst-case-vllm`: the Responses body without tools, `include`,
//! `max_tool_calls` and `metadata`) and the Responses translation with the
//! `<think>` handling of `inst-vllm-think`.

use serde_json::{Value, json};
use uuid::Uuid;

use super::*;
use crate::domain::service::stream::context::CurrentMessage;
use crate::domain::service::stream::inputs::ImageAttachment;
use crate::domain::service::stream::types::ToolName;
use crate::infra::llm::providers::test_fixtures::{
    Parts, all_tools, header, keys, named, outputs, plan_with_history, request_with, run_stream,
    usage, user_hex,
};
use crate::test_doubles::LogCapture;

fn parts() -> Parts {
    Parts::new(ProviderKind::VllmResponses, "vllm.test", "/v1/responses")
}

fn adapter() -> VllmResponsesAdapter {
    VllmResponsesAdapter::new()
}

fn build(parts: &Parts) -> BuiltRequest {
    parts.build_with(&adapter())
}

fn body(parts: &Parts) -> Value {
    build(parts).body
}

// ==== the request ================================================================================

#[test]
fn the_minimal_request_is_exactly_this_body() {
    assert_eq!(
        body(&parts()),
        json!({
            "model": "model-test",
            "instructions": "SYS",
            "input": [
                { "role": "user", "content": [ { "type": "input_text", "text": "hello" } ] }
            ],
            "max_output_tokens": 500,
            "stream": true,
            "store": false,
            "user": user_hex()
        })
    );
}

#[test]
fn the_adapter_serves_the_vllm_kind() {
    assert_eq!(adapter().kind(), ProviderKind::VllmResponses);
    assert_eq!(build(&parts()).kind, ProviderKind::VllmResponses);
}

#[test]
fn the_uri_is_the_alias_and_the_api_path_with_two_headers_and_no_credential() {
    let built = build(&parts());
    assert_eq!(built.uri, "/vllm.test/v1/responses");
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
            ("content-type".to_owned(), "application/json".to_owned()),
        ]
    );
    assert_eq!(header(&built, "content-type"), Some("application/json"));
}

#[test]
fn every_tool_is_dropped_function_tools_included() {
    let mut parts = parts();
    parts.set_tools(all_tools(), "file_search+web_search+code_interpreter");
    let body = body(&parts);
    for key in [
        "tools",
        "include",
        "max_tool_calls",
        "metadata",
        "tool_choice",
    ] {
        assert!(body.get(key).is_none(), "`{key}` must not be sent");
    }
    // Nothing else changes: the same body as without tools.
    assert_eq!(body, self::body(&self::parts()));
}

#[test]
fn the_summary_the_history_and_the_current_message_use_the_responses_parts() {
    let mut parts = parts();
    parts.plan = plan_with_history();
    assert_eq!(
        body(&parts)["input"],
        json!([
            { "role": "user", "content": [ { "type": "input_text", "text": "PREAMBLE\n\nthe summary" } ] },
            { "role": "user", "content": [ { "type": "input_text", "text": "q1" } ] },
            { "role": "assistant", "content": [ { "type": "output_text", "text": "a1" } ] },
            { "role": "user", "content": [ { "type": "input_text", "text": "hello" } ] }
        ])
    );
}

#[test]
fn an_image_is_an_input_image_with_its_provider_file_id() {
    let mut parts = parts();
    parts.plan.current = Some(CurrentMessage {
        content: "look".to_owned(),
        images: vec![ImageAttachment {
            id: Uuid::from_u128(1),
            provider_file_id: Some("file-img1".to_owned()),
            secondary_file_id: None,
        }],
    });
    assert_eq!(
        body(&parts)["input"][0]["content"],
        json!([
            { "type": "input_text", "text": "look" },
            { "type": "input_image", "file_id": "file-img1" }
        ])
    );
}

#[test]
fn instructions_are_omitted_when_empty() {
    let mut parts = parts();
    parts.plan.instructions = String::new();
    assert!(body(&parts).get("instructions").is_none());
}

#[test]
fn sampling_parameters_and_the_reasoning_effort_are_sent_only_when_set() {
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
    assert_eq!(body["frequency_penalty"], 0.1);
    assert_eq!(body["presence_penalty"], 0.3);
    assert_eq!(body["reasoning"], json!({ "effort": "high" }));
}

#[test]
fn stop_sequences_are_never_sent() {
    let mut parts = parts();
    parts.entry.general_config.api_params.stop = vec!["END".to_owned()];
    assert!(body(&parts).get("stop").is_none());
}

#[test]
fn extra_body_keys_are_merged_except_the_controlled_ones_which_warn_once_each() {
    let logs = LogCapture::start();
    let mut parts = parts();
    let controlled = [
        "model",
        "input",
        "messages",
        "instructions",
        "system",
        "stream",
        "stream_options",
        "max_output_tokens",
        "max_completion_tokens",
        "max_tokens",
        "max_tool_calls",
        "tools",
        "tool_choice",
        "include",
        "store",
        "previous_response_id",
        "user",
        "metadata",
    ];
    let mut extra = serde_json::Map::new();
    for key in controlled {
        extra.insert(key.to_owned(), json!("evil"));
    }
    extra.insert("top_k".to_owned(), json!(40));
    parts.entry.general_config.api_params.extra_body = Some(Value::Object(extra));
    let mut expected = body(&self::parts());
    expected["top_k"] = json!(40);
    assert_eq!(body(&parts), expected);
    let warnings = logs.at(tracing::Level::WARN);
    assert_eq!(warnings.len(), controlled.len());
    for key in controlled {
        assert!(warnings.iter().any(|w| w.text().contains(key)), "`{key}`");
    }
}

#[test]
fn the_full_request_has_no_key_beyond_the_contract() {
    let mut parts = parts();
    let params = &mut parts.entry.general_config.api_params;
    params.temperature = Some(0.5);
    params.top_p = Some(1.0);
    params.frequency_penalty = Some(0.0);
    params.presence_penalty = Some(0.0);
    params.reasoning_effort = Some("low".to_owned());
    parts.set_tools(all_tools(), "file_search+web_search+code_interpreter");
    assert_eq!(
        keys(&body(&parts)),
        [
            "frequency_penalty",
            "input",
            "instructions",
            "max_output_tokens",
            "model",
            "presence_penalty",
            "reasoning",
            "store",
            "stream",
            "temperature",
            "top_p",
            "user"
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

// ==== the continuation ===========================================================================

#[test]
fn the_continuation_appends_function_call_and_output_items_to_the_input() {
    let previous = build(&parts());
    let next = adapter().continue_request(&previous, &outputs()).unwrap();
    let mut expected = previous.body.clone();
    let input = expected["input"].as_array_mut().unwrap();
    input.push(json!({
        "type": "function_call", "call_id": "call_1", "name": "search_knowledge",
        "arguments": "{\"query\":\"alpha\"}"
    }));
    input.push(json!({
        "type": "function_call_output", "call_id": "call_1",
        "output": "{\"results\":[{\"text\":\"A\"}]}"
    }));
    input.push(json!({
        "type": "function_call", "call_id": "call_2", "name": "search_knowledge",
        "arguments": "{\"query\":\"beta\"}"
    }));
    input.push(json!({
        "type": "function_call_output", "call_id": "call_2",
        "output": "{\"error\":\"search limit reached\"}"
    }));
    assert_eq!(next.body, expected);
    assert_eq!(next.kind, ProviderKind::VllmResponses);
    assert_eq!(next.uri, previous.uri);
}

#[test]
fn a_body_without_input_cannot_be_continued() {
    let broken = request_with(ProviderKind::VllmResponses, json!({ "model": "m" }));
    assert!(matches!(
        adapter().continue_request(&broken, &outputs()),
        Err(DomainError::Internal(_))
    ));
}

// ==== the translation ============================================================================

fn delta(text: &str) -> SseEvent {
    named(
        "response.output_text.delta",
        json!({
            "type": "response.output_text.delta",
            "item_id": "msg_1", "content_index": 0, "delta": text
        }),
    )
}

fn text(text: &str) -> ProviderEvent {
    ProviderEvent::TextDelta {
        text: text.to_owned(),
        item_id: Some("msg_1".to_owned()),
        content_index: Some(0),
    }
}

fn reasoning(text: &str) -> ProviderEvent {
    ProviderEvent::ReasoningDelta {
        text: text.to_owned(),
    }
}

fn completed_event() -> SseEvent {
    named(
        "response.completed",
        json!({ "response": {
            "id": "resp_1",
            "usage": { "input_tokens": 11, "output_tokens": 7,
                       "input_tokens_details": { "cached_tokens": 2 },
                       "output_tokens_details": { "reasoning_tokens": 3 } },
            "output": []
        } }),
    )
}

fn completed() -> ProviderEvent {
    ProviderEvent::Completed {
        usage: Some(usage(11, 7, 2, 0, 3)),
        response_id: Some("resp_1".to_owned()),
        incomplete_reason: None,
    }
}

fn translate_all(events: &[SseEvent]) -> Vec<ProviderEvent> {
    run_stream(&mut *adapter().translator(), events)
}

#[test]
fn the_responses_table_applies_text_completion_and_usage() {
    assert_eq!(
        translate_all(&[delta("Hel"), delta("lo"), completed_event()]),
        vec![text("Hel"), text("lo"), completed()]
    );
}

#[test]
fn the_event_name_comes_from_the_type_when_the_line_is_missing_or_message() {
    let data = json!({ "type": "response.output_text.delta", "delta": "x" }).to_string();
    for name in [None, Some("message")] {
        let event = SseEvent {
            event: name.map(str::to_owned),
            data: data.clone(),
            id: None,
        };
        assert_eq!(
            translate_all(&[event]),
            vec![ProviderEvent::TextDelta {
                text: "x".to_owned(),
                item_id: None,
                content_index: None,
            }],
            "{name:?}"
        );
    }
}

#[test]
fn an_empty_event_name_is_treated_as_message() {
    let event = SseEvent {
        event: Some(String::new()),
        data: json!({ "type": "response.output_text.delta", "delta": "x" }).to_string(),
        id: None,
    };
    assert_eq!(
        translate_all(&[event]),
        vec![ProviderEvent::TextDelta {
            text: "x".to_owned(),
            item_id: None,
            content_index: None,
        }]
    );
}

#[test]
fn a_failed_response_and_an_error_event_are_provider_errors() {
    let failed = named(
        "response.failed",
        json!({ "response": { "error": { "code": "server_error", "message": "boom" },
                              "usage": { "input_tokens": 4, "output_tokens": 0 } } }),
    );
    assert_eq!(
        translate_all(&[failed]),
        vec![ProviderEvent::ProviderError {
            code: Some("server_error".to_owned()),
            message: "boom".to_owned(),
            usage: Some(usage(4, 0, 0, 0, 0)),
        }]
    );
    let error = named(
        "error",
        json!({ "code": "rate_limit", "message": "slow down" }),
    );
    assert_eq!(
        translate_all(&[error]),
        vec![ProviderEvent::ProviderError {
            code: Some("rate_limit".to_owned()),
            message: "slow down".to_owned(),
            usage: None,
        }]
    );
}

#[test]
fn an_incomplete_response_completes_with_its_reason() {
    let event = named(
        "response.incomplete",
        json!({ "response": { "id": "resp_1", "incomplete_details": { "reason": "max_output_tokens" },
                              "usage": { "input_tokens": 1, "output_tokens": 2 } } }),
    );
    assert_eq!(
        translate_all(&[event]),
        vec![ProviderEvent::Completed {
            usage: Some(usage(1, 2, 0, 0, 0)),
            response_id: Some("resp_1".to_owned()),
            incomplete_reason: Some("max_output_tokens".to_owned()),
        }]
    );
}

#[test]
fn tool_events_and_unknown_events_follow_the_responses_table() {
    let events = [
        named("response.created", json!({})),
        named("response.web_search_call.searching", json!({})),
        named("response.output_text.done", json!({})),
    ];
    assert_eq!(
        translate_all(&events),
        vec![ProviderEvent::ToolStart {
            name: ToolName::WebSearch,
            details: json!({}),
        }]
    );
}

// ---- <think> ------------------------------------------------------------------------------------

#[test]
fn text_inside_the_think_tags_is_reasoning_and_the_tags_are_removed() {
    assert_eq!(
        translate_all(&[delta("<think>plan the answer</think>The answer.")]),
        vec![reasoning("plan the answer"), text("The answer.")]
    );
}

#[test]
fn a_stream_without_tags_is_plain_text() {
    assert_eq!(
        translate_all(&[delta("Hello "), delta("world")]),
        vec![text("Hello "), text("world")]
    );
}

#[test]
fn tags_that_arrive_as_deltas_of_their_own_emit_nothing() {
    assert_eq!(
        translate_all(&[
            delta("<think>"),
            delta("step 1"),
            delta(" step 2"),
            delta("</think>"),
            delta("Hello"),
        ]),
        vec![reasoning("step 1"), reasoning(" step 2"), text("Hello")]
    );
}

#[test]
fn a_tag_split_across_deltas_is_recognized() {
    assert_eq!(
        translate_all(&[delta("<thi"), delta("nk>abc</th"), delta("ink>def")]),
        vec![reasoning("abc"), text("def")]
    );
}

#[test]
fn text_before_and_between_think_blocks_is_text() {
    assert_eq!(
        translate_all(&[delta("Hi <think>x</think> there<think>y</think>!")]),
        vec![
            text("Hi "),
            reasoning("x"),
            text(" there"),
            reasoning("y"),
            text("!"),
        ]
    );
}

#[test]
fn a_held_back_prefix_that_is_not_a_tag_is_emitted_with_the_next_delta() {
    assert_eq!(
        translate_all(&[delta("a<"), delta("b")]),
        vec![text("a"), text("<b")]
    );
}

#[test]
fn an_angle_bracket_that_is_not_a_tag_stays_text_and_reasoning() {
    assert_eq!(
        translate_all(&[delta("1 < 2 and <b>bold</b>")]),
        vec![text("1 < 2 and <b>bold</b>")]
    );
    assert_eq!(
        translate_all(&[delta("<think>a < b</think>done")]),
        vec![reasoning("a < b"), text("done")]
    );
}

#[test]
fn a_closing_tag_outside_a_think_block_is_literal_text() {
    assert_eq!(
        translate_all(&[delta("x</think>y")]),
        vec![text("x</think>y")]
    );
}

#[test]
fn a_pending_prefix_is_flushed_before_the_completion() {
    assert_eq!(
        translate_all(&[delta("abc<th"), completed_event()]),
        vec![text("abc"), text("<th"), completed()]
    );
}

#[test]
fn an_unclosed_think_block_is_reasoning_to_the_end() {
    assert_eq!(
        translate_all(&[delta("<think>abc"), completed_event()]),
        vec![reasoning("abc"), completed()]
    );
}

#[test]
fn a_pending_prefix_is_flushed_before_a_provider_error() {
    let failed = named(
        "response.failed",
        json!({ "response": { "error": { "code": "x", "message": "m" } } }),
    );
    let out = translate_all(&[delta("ab<"), failed]);
    assert_eq!(out[0], text("ab"));
    assert_eq!(out[1], text("<"));
    assert!(matches!(out[2], ProviderEvent::ProviderError { .. }));
}

#[test]
fn the_end_of_the_body_flushes_a_pending_prefix() {
    let mut translator = adapter().translator();
    assert_eq!(translator.translate(&delta("ab<thi")), vec![text("ab")]);
    assert_eq!(translator.finish(), vec![text("<thi")]);
    assert!(translator.finish().is_empty());
}

#[test]
fn empty_segments_are_never_emitted() {
    assert_eq!(
        translate_all(&[delta("<think></think>"), delta(""), delta("x")]),
        vec![text("x")]
    );
}

#[test]
fn reasoning_events_carry_no_ids_and_text_keeps_the_ids_of_its_delta() {
    let out = translate_all(&[delta("<think>r</think>t")]);
    assert_eq!(
        out,
        vec![
            ProviderEvent::ReasoningDelta {
                text: "r".to_owned()
            },
            ProviderEvent::TextDelta {
                text: "t".to_owned(),
                item_id: Some("msg_1".to_owned()),
                content_index: Some(0),
            },
        ]
    );
}

#[test]
fn each_translator_has_its_own_think_state() {
    let adapter = adapter();
    let mut first = adapter.translator();
    let mut second = adapter.translator();
    assert_eq!(first.translate(&delta("<think>a")), vec![reasoning("a")]);
    assert_eq!(second.translate(&delta("b")), vec![text("b")]);
}
