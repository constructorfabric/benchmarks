#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::redundant_clone, clippy::non_ascii_literal)]
//! Slice 7: the `openai_chat_completions` adapter: the golden request body
//! (`cpt-cf-mini-chat-algo-streaming-turns-build-provider-request`, case
//! `inst-case-chat-completions`), the continuation of the knowledge-search
//! loop and the translation of scripted chunk streams
//! (`cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream`,
//! `inst-chat-completions`).

use serde_json::{Value, json};
use uuid::Uuid;

use super::*;
use crate::domain::service::stream::context::CurrentMessage;
use crate::domain::service::stream::inputs::ImageAttachment;
use crate::domain::service::stream::ports::HistoryRole;
use crate::domain::service::stream::tools::ToolSpec;
use crate::domain::service::stream::types::ToolName;
use crate::infra::llm::providers::test_fixtures::{
    Parts, all_tools, call, file_search, header, history, keys, outputs, plan_with_history, raw,
    request_with, run_stream, text_delta, unnamed, usage, user_hex,
};
use crate::test_doubles::LogCapture;

fn parts() -> Parts {
    Parts::new(
        ProviderKind::OpenaiChatCompletions,
        "api.openai.test",
        "/v1/chat/completions",
    )
}

fn adapter() -> ChatCompletionsAdapter {
    ChatCompletionsAdapter::new()
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
            "messages": [
                { "role": "system", "content": "SYS" },
                { "role": "user", "content": "hello" }
            ],
            "stream": true,
            "stream_options": { "include_usage": true },
            "max_completion_tokens": 500,
            "user": user_hex()
        })
    );
}

#[test]
fn the_adapter_serves_the_chat_completions_kind() {
    assert_eq!(adapter().kind(), ProviderKind::OpenaiChatCompletions);
    assert_eq!(
        build(&parts()).kind,
        ProviderKind::OpenaiChatCompletions,
        "the kind of the built request selects the translator"
    );
}

#[test]
fn the_uri_is_the_alias_and_the_api_path_with_two_headers_and_no_credential() {
    let built = build(&parts());
    assert_eq!(built.uri, "/api.openai.test/v1/chat/completions");
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
}

#[test]
fn the_model_placeholder_and_the_query_of_the_api_path_are_kept() {
    let mut parts = parts();
    parts.target.api_path =
        "/openai/deployments/{model}/chat/completions?api-version=2025-01-01".to_owned();
    assert_eq!(
        build(&parts).uri,
        "/api.openai.test/openai/deployments/model-test/chat/completions?api-version=2025-01-01"
    );
}

#[test]
fn the_user_value_is_the_two_simple_uuids_tenant_first() {
    let body = body(&parts());
    let user = body["user"].as_str().unwrap();
    assert_eq!(user.len(), 64);
    assert_eq!(user, user_hex());
}

#[test]
fn the_system_message_is_omitted_when_the_instructions_are_empty() {
    let mut parts = parts();
    parts.plan.instructions = String::new();
    assert_eq!(
        body(&parts)["messages"],
        json!([{ "role": "user", "content": "hello" }])
    );
}

#[test]
fn the_system_message_carries_the_prompt_and_the_guards() {
    let mut parts = parts();
    parts.plan.instructions = "SYS\n\nKS-GUARD".to_owned();
    assert_eq!(
        body(&parts)["messages"][0],
        json!({ "role": "system", "content": "SYS\n\nKS-GUARD" })
    );
}

#[test]
fn the_messages_are_the_summary_the_history_and_the_current_message_with_string_content() {
    let mut parts = parts();
    parts.plan = plan_with_history();
    assert_eq!(
        body(&parts)["messages"],
        json!([
            { "role": "system", "content": "SYS" },
            { "role": "user", "content": "PREAMBLE\n\nthe summary" },
            { "role": "user", "content": "q1" },
            { "role": "assistant", "content": "a1" },
            { "role": "user", "content": "hello" }
        ])
    );
}

#[test]
fn images_are_never_sent() {
    let mut parts = parts();
    parts.plan.current = Some(CurrentMessage {
        content: "look".to_owned(),
        images: vec![ImageAttachment {
            id: Uuid::from_u128(1),
            provider_file_id: Some("file-img1".to_owned()),
            secondary_file_id: Some("sec-1".to_owned()),
        }],
    });
    let body = body(&parts);
    assert_eq!(
        body["messages"][1],
        json!({ "role": "user", "content": "look" }),
        "the current message stays a string"
    );
    let text = body.to_string();
    for forbidden in ["image_url", "file-img1", "sec-1", "input_image"] {
        assert!(!text.contains(forbidden), "`{forbidden}` in {text}");
    }
}

// ---- tools --------------------------------------------------------------------------------------

#[test]
fn only_the_search_knowledge_function_tool_is_kept() {
    let mut parts = parts();
    parts.set_tools(all_tools(), "file_search+web_search+code_interpreter");
    let body = body(&parts);
    let tools = body["tools"].as_array().unwrap();
    assert_eq!(
        tools.len(),
        1,
        "file_search, web_search and code_interpreter are dropped"
    );
    assert_eq!(tools[0]["type"], "function");
    let function = &tools[0]["function"];
    assert_eq!(function["name"], "search_knowledge");
    assert!(
        function["description"]
            .as_str()
            .is_some_and(|d| !d.is_empty())
    );
    assert_eq!(
        function["parameters"],
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "What to search for." },
                "top_k": { "type": "integer", "description": "How many passages to return." }
            },
            "required": ["query"]
        })
    );
    assert!(
        tools[0].get("name").is_none(),
        "Chat Completions nests the definition under `function`"
    );
}

#[test]
fn tools_are_absent_when_only_dropped_tools_were_decided() {
    let mut parts = parts();
    parts.set_tools(
        vec![
            file_search(),
            ToolSpec::WebSearch {
                search_context_size: mini_chat_sdk::WebSearchContextSize::Low,
            },
        ],
        "file_search+web_search",
    );
    assert!(body(&parts).get("tools").is_none());
    parts.set_tools(Vec::new(), "none");
    assert!(body(&parts).get("tools").is_none());
}

#[test]
fn the_responses_only_keys_are_never_sent() {
    let mut parts = parts();
    parts.set_tools(all_tools(), "file_search+web_search+code_interpreter");
    let body = body(&parts);
    for key in [
        "input",
        "instructions",
        "max_output_tokens",
        "max_tokens",
        "max_tool_calls",
        "include",
        "store",
        "metadata",
        "tool_choice",
        "previous_response_id",
        "reasoning",
    ] {
        assert!(body.get(key).is_none(), "`{key}` must not be sent");
    }
}

// ---- parameters and extra_body ------------------------------------------------------------------

#[test]
fn the_output_limit_is_max_completion_tokens() {
    let mut parts = parts();
    parts.max_output = 1234;
    let body = body(&parts);
    assert_eq!(body["max_completion_tokens"], 1234);
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"], json!({ "include_usage": true }));
}

#[test]
fn stop_sequences_are_sent_only_when_not_empty() {
    let mut parts = parts();
    assert!(body(&parts).get("stop").is_none());
    parts.entry.general_config.api_params.stop = vec!["END".to_owned(), "STOP".to_owned()];
    assert_eq!(body(&parts)["stop"], json!(["END", "STOP"]));
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
    assert_eq!(body["reasoning_effort"], "high");
    assert!(body.get("reasoning").is_none(), "not the Responses form");

    let plain = self::body(&self::parts());
    for key in [
        "temperature",
        "top_p",
        "frequency_penalty",
        "presence_penalty",
        "reasoning_effort",
    ] {
        assert!(plain.get(key).is_none(), "{key}");
    }
}

#[test]
fn extra_body_keys_are_merged_at_the_top_level() {
    let mut parts = parts();
    parts.entry.general_config.api_params.extra_body =
        Some(json!({ "seed": 7, "logit_bias": { "50256": -100 } }));
    let body = body(&parts);
    assert_eq!(body["seed"], 7);
    assert_eq!(body["logit_bias"], json!({ "50256": -100 }));
}

#[test]
fn controlled_extra_body_keys_are_ignored_with_one_warning_each() {
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
    extra.insert("seed".to_owned(), json!(1));
    parts.entry.general_config.api_params.extra_body = Some(Value::Object(extra));
    let mut expected = body(&self::parts());
    expected["seed"] = json!(1);
    assert_eq!(body(&parts), expected);
    let warnings = logs.at(tracing::Level::WARN);
    assert_eq!(
        warnings.len(),
        controlled.len(),
        "one warning per ignored key"
    );
    for key in controlled {
        assert!(
            warnings.iter().any(|w| w.text().contains(key)),
            "no warning names `{key}`"
        );
    }
}

#[test]
fn a_non_object_extra_body_is_ignored() {
    let mut parts = parts();
    parts.entry.general_config.api_params.extra_body = Some(json!(["a"]));
    assert_eq!(body(&parts), body(&self::parts()));
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
        "api-key",
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
    params.frequency_penalty = Some(0.0);
    params.presence_penalty = Some(0.0);
    params.reasoning_effort = Some("low".to_owned());
    params.stop = vec!["END".to_owned()];
    parts.set_tools(vec![ToolSpec::SearchKnowledge], "none");
    assert_eq!(
        keys(&body(&parts)),
        [
            "frequency_penalty",
            "max_completion_tokens",
            "messages",
            "model",
            "presence_penalty",
            "reasoning_effort",
            "stop",
            "stream",
            "stream_options",
            "temperature",
            "tools",
            "top_p",
            "user"
        ]
    );
}

#[test]
fn identical_inputs_give_byte_identical_bodies() {
    let mut parts = parts();
    parts.plan = plan_with_history();
    let first = serde_json::to_vec(&body(&parts)).unwrap();
    let second = serde_json::to_vec(&body(&parts)).unwrap();
    assert_eq!(first, second);
}

// ==== the continuation of the knowledge-search loop ==============================================

fn previous() -> BuiltRequest {
    let mut parts = parts();
    parts.set_tools(vec![ToolSpec::SearchKnowledge], "none");
    build(&parts)
}

#[test]
fn the_continuation_appends_one_assistant_message_with_the_calls_and_one_tool_message_per_call() {
    let previous = previous();
    let next = adapter().continue_request(&previous, &outputs()).unwrap();
    let mut expected = previous.body.clone();
    let messages = expected["messages"].as_array_mut().unwrap();
    messages.push(json!({
        "role": "assistant",
        "content": null,
        "tool_calls": [
            { "id": "call_1", "type": "function",
              "function": { "name": "search_knowledge", "arguments": "{\"query\":\"alpha\"}" } },
            { "id": "call_2", "type": "function",
              "function": { "name": "search_knowledge", "arguments": "{\"query\":\"beta\"}" } }
        ]
    }));
    messages.push(json!({
        "role": "tool", "tool_call_id": "call_1", "content": "{\"results\":[{\"text\":\"A\"}]}"
    }));
    messages.push(json!({
        "role": "tool", "tool_call_id": "call_2", "content": "{\"error\":\"search limit reached\"}"
    }));
    assert_eq!(next.body, expected);
}

#[test]
fn the_continuation_keeps_the_tools_the_target_the_headers_and_the_kind() {
    let previous = previous();
    let next = adapter().continue_request(&previous, &outputs()).unwrap();
    assert_eq!(next.kind, previous.kind);
    assert_eq!(next.uri, previous.uri);
    assert_eq!(next.headers, previous.headers);
    assert_eq!(next.body["tools"], previous.body["tools"]);
    assert_eq!(next.body["stream_options"], previous.body["stream_options"]);
    assert_eq!(next.body["max_completion_tokens"], 500);
}

#[test]
fn a_second_continuation_appends_after_the_first() {
    let first = adapter().continue_request(&previous(), &outputs()).unwrap();
    let second = adapter().continue_request(&first, &outputs()[..1]).unwrap();
    let messages = second.body["messages"].as_array().unwrap();
    assert_eq!(
        messages.len(),
        2 + 3 + 2,
        "system, user, 3 of the first round, 2 of the second"
    );
    assert_eq!(messages[5]["tool_calls"].as_array().unwrap().len(), 1);
    assert_eq!(messages[6]["role"], "tool");
}

#[test]
fn the_previous_request_is_not_changed() {
    let previous = previous();
    let before = previous.clone();
    let _ = adapter().continue_request(&previous, &outputs()).unwrap();
    assert_eq!(previous, before);
}

#[test]
fn a_body_without_messages_cannot_be_continued() {
    let broken = request_with(ProviderKind::OpenaiChatCompletions, json!({ "model": "m" }));
    let error = adapter().continue_request(&broken, &outputs()).unwrap_err();
    assert!(matches!(error, DomainError::Internal(_)), "{error:?}");
}

// ==== the translation ============================================================================

fn chunk(delta: &Value, finish: Option<&str>) -> SseEvent {
    unnamed(json!({
        "id": "chatcmpl-1",
        "object": "chat.completion.chunk",
        "choices": [ { "index": 0, "delta": delta, "finish_reason": finish } ],
        "usage": null
    }))
}

fn usage_chunk(prompt: u64, completion: u64) -> SseEvent {
    unnamed(json!({
        "id": "chatcmpl-1",
        "object": "chat.completion.chunk",
        "choices": [],
        "usage": {
            "prompt_tokens": prompt,
            "completion_tokens": completion,
            "total_tokens": prompt + completion,
            "prompt_tokens_details": { "cached_tokens": 4 },
            "completion_tokens_details": { "reasoning_tokens": 1 }
        }
    }))
}

fn translate_all(events: &[SseEvent]) -> Vec<ProviderEvent> {
    run_stream(&mut *ChatCompletionsAdapter::new().translator(), events)
}

fn completed(usage: Option<mini_chat_sdk::UsageTokens>, reason: Option<&str>) -> ProviderEvent {
    ProviderEvent::Completed {
        usage,
        response_id: Some("chatcmpl-1".to_owned()),
        incomplete_reason: reason.map(str::to_owned),
    }
}

#[test]
fn a_whole_stream_is_text_deltas_then_one_completion_with_the_usage() {
    let events = [
        chunk(&json!({ "role": "assistant", "content": "" }), None),
        chunk(&json!({ "content": "Hel" }), None),
        chunk(&json!({ "content": "lo" }), None),
        chunk(&json!({}), Some("stop")),
        usage_chunk(12, 3),
        raw("[DONE]"),
    ];
    assert_eq!(
        translate_all(&events),
        vec![
            text_delta("Hel"),
            text_delta("lo"),
            completed(Some(usage(12, 3, 4, 0, 1)), None),
        ]
    );
}

#[test]
fn the_completion_waits_for_the_usage_chunk() {
    let mut translator = adapter().translator();
    assert_eq!(
        translator.translate(&chunk(&json!({ "content": "x" }), None)),
        vec![text_delta("x")]
    );
    assert!(
        translator
            .translate(&chunk(&json!({}), Some("stop")))
            .is_empty(),
        "the finish reason only marks the completion"
    );
    assert_eq!(
        translator.translate(&usage_chunk(5, 2)),
        vec![completed(Some(usage(5, 2, 4, 0, 1)), None)],
        "the usage chunk completes it"
    );
    assert!(translator.translate(&raw("[DONE]")).is_empty());
    assert!(translator.finish().is_empty(), "nothing is released twice");
}

#[test]
fn done_releases_the_pending_completion_without_usage() {
    let events = [
        chunk(&json!({ "content": "x" }), None),
        chunk(&json!({}), Some("stop")),
        raw("[DONE]"),
    ];
    assert_eq!(
        translate_all(&events),
        vec![text_delta("x"), completed(None, None)]
    );
}

#[test]
fn the_end_of_the_body_releases_a_pending_completion_when_done_never_came() {
    let mut translator = adapter().translator();
    assert!(
        translator
            .translate(&chunk(&json!({}), Some("stop")))
            .is_empty()
    );
    assert_eq!(translator.finish(), vec![completed(None, None)]);
    assert!(translator.finish().is_empty());
}

#[test]
fn a_stream_without_a_finish_reason_has_no_outcome() {
    let events = [chunk(&json!({ "content": "x" }), None), raw("[DONE]")];
    assert_eq!(translate_all(&events), vec![text_delta("x")]);
}

#[test]
fn usage_in_the_finish_chunk_completes_it_at_once() {
    let mut translator = adapter().translator();
    let event = unnamed(json!({
        "id": "chatcmpl-1",
        "choices": [ { "index": 0, "delta": {}, "finish_reason": "stop" } ],
        "usage": { "prompt_tokens": 7, "completion_tokens": 2 }
    }));
    assert_eq!(
        translator.translate(&event),
        vec![completed(Some(usage(7, 2, 0, 0, 0)), None)]
    );
}

#[test]
fn usage_before_the_finish_reason_completes_at_the_finish_reason() {
    let mut translator = adapter().translator();
    assert!(translator.translate(&usage_chunk(5, 2)).is_empty());
    assert_eq!(
        translator.translate(&chunk(&json!({}), Some("stop"))),
        vec![completed(Some(usage(5, 2, 4, 0, 1)), None)]
    );
}

#[test]
fn missing_usage_fields_are_zero() {
    let events = [
        chunk(&json!({}), Some("stop")),
        unnamed(json!({ "choices": [], "usage": { "prompt_tokens": 9 } })),
    ];
    assert_eq!(
        translate_all(&events),
        vec![completed(Some(usage(9, 0, 0, 0, 0)), None)]
    );
}

#[test]
fn the_finish_reasons_map_to_completion_reasons() {
    for (finish, reason) in [
        ("stop", None),
        ("length", Some("max_tokens")),
        ("content_filter", Some("content_filter")),
    ] {
        let events = [chunk(&json!({}), Some(finish)), raw("[DONE]")];
        assert_eq!(
            translate_all(&events),
            vec![completed(None, reason)],
            "{finish}"
        );
    }
}

#[test]
fn an_unknown_finish_reason_completes_with_that_reason() {
    let events = [
        chunk(&json!({}), Some("insufficient_system_resource")),
        raw("[DONE]"),
    ];
    assert_eq!(
        translate_all(&events),
        vec![completed(None, Some("insufficient_system_resource"))]
    );
}

#[test]
fn a_null_finish_reason_and_empty_deltas_are_ignored() {
    let events = [
        chunk(&json!({}), None),
        chunk(&json!({ "content": null }), None),
        chunk(&json!({ "role": "assistant" }), None),
        unnamed(json!({ "choices": [] })),
    ];
    assert!(translate_all(&events).is_empty());
}

#[test]
fn a_tool_call_is_started_on_its_first_fragment_and_done_at_the_finish_reason() {
    let mut translator = adapter().translator();
    assert_eq!(
        translator.translate(&chunk(
            &json!({ "tool_calls": [
                { "index": 0, "id": "call_1", "type": "function",
                  "function": { "name": "search_knowledge", "arguments": "" } }
            ] }),
            None
        )),
        vec![ProviderEvent::ToolStart {
            name: ToolName::FunctionCall,
            details: json!({ "index": 0, "call_id": "call_1", "name": "search_knowledge" }),
        }]
    );
    for fragment in ["{\"que", "ry\":\"x\"}"] {
        assert!(
            translator
                .translate(&chunk(
                    &json!({ "tool_calls": [ { "index": 0, "function": { "arguments": fragment } } ] }),
                    None
                ))
                .is_empty(),
            "later fragments only accumulate"
        );
    }
    assert_eq!(
        translator.translate(&chunk(&json!({}), Some("tool_calls"))),
        vec![ProviderEvent::ToolDone {
            name: ToolName::FunctionCall,
            details: json!({
                "call_id": "call_1",
                "name": "search_knowledge",
                "arguments": "{\"query\":\"x\"}"
            }),
        }]
    );
    assert_eq!(
        translator.translate(&usage_chunk(30, 4)),
        vec![ProviderEvent::ToolUse {
            calls: vec![call("call_1", "search_knowledge", "{\"query\":\"x\"}")],
            usage: Some(usage(30, 4, 4, 0, 1)),
        }]
    );
}

#[test]
fn parallel_tool_calls_are_started_in_arrival_order_and_finished_by_index() {
    let events = [
        chunk(
            &json!({ "tool_calls": [
                { "index": 0, "id": "call_a", "type": "function",
                  "function": { "name": "search_knowledge", "arguments": "{\"query\"" } }
            ] }),
            None,
        ),
        chunk(
            &json!({ "tool_calls": [
                { "index": 1, "id": "call_b", "type": "function",
                  "function": { "name": "load_files", "arguments": "{}" } }
            ] }),
            None,
        ),
        chunk(
            &json!({ "tool_calls": [ { "index": 0, "function": { "arguments": ":\"a\"}" } } ] }),
            None,
        ),
        chunk(&json!({}), Some("tool_calls")),
        usage_chunk(8, 2),
        raw("[DONE]"),
    ];
    assert_eq!(
        translate_all(&events),
        vec![
            ProviderEvent::ToolStart {
                name: ToolName::FunctionCall,
                details: json!({ "index": 0, "call_id": "call_a", "name": "search_knowledge" }),
            },
            ProviderEvent::ToolStart {
                name: ToolName::FunctionCall,
                details: json!({ "index": 1, "call_id": "call_b", "name": "load_files" }),
            },
            ProviderEvent::ToolDone {
                name: ToolName::FunctionCall,
                details: json!({
                    "call_id": "call_a", "name": "search_knowledge", "arguments": "{\"query\":\"a\"}"
                }),
            },
            ProviderEvent::ToolDone {
                name: ToolName::FunctionCall,
                details: json!({ "call_id": "call_b", "name": "load_files", "arguments": "{}" }),
            },
            ProviderEvent::ToolUse {
                calls: vec![
                    call("call_a", "search_knowledge", "{\"query\":\"a\"}"),
                    call("call_b", "load_files", "{}"),
                ],
                usage: Some(usage(8, 2, 4, 0, 1)),
            },
        ]
    );
}

#[test]
fn text_before_a_tool_call_is_relayed_and_a_tool_use_without_usage_is_released_by_done() {
    let events = [
        chunk(&json!({ "content": "Let me check." }), None),
        chunk(
            &json!({ "tool_calls": [
                { "index": 0, "id": "c1", "type": "function",
                  "function": { "name": "search_knowledge", "arguments": "{}" } }
            ] }),
            None,
        ),
        chunk(&json!({}), Some("tool_calls")),
        raw("[DONE]"),
    ];
    let out = translate_all(&events);
    assert_eq!(out[0], text_delta("Let me check."));
    assert_eq!(
        out.last().unwrap(),
        &ProviderEvent::ToolUse {
            calls: vec![call("c1", "search_knowledge", "{}")],
            usage: None,
        }
    );
    assert_eq!(out.len(), 4, "text, start, done, tool use: {out:?}");
}

#[test]
fn a_tool_call_without_arguments_has_empty_arguments_text() {
    let events = [
        chunk(
            &json!({ "tool_calls": [
                { "index": 0, "id": "c1", "type": "function", "function": { "name": "load_files" } }
            ] }),
            None,
        ),
        chunk(&json!({}), Some("tool_calls")),
        raw("[DONE]"),
    ];
    let out = translate_all(&events);
    assert_eq!(
        out.last().unwrap(),
        &ProviderEvent::ToolUse {
            calls: vec![call("c1", "load_files", "")],
            usage: None,
        }
    );
}

#[test]
fn an_error_object_is_a_provider_error_with_its_code_and_message() {
    let events = [unnamed(json!({
        "error": { "message": "The server had an error", "type": "server_error", "code": "overloaded" }
    }))];
    assert_eq!(
        translate_all(&events),
        vec![ProviderEvent::ProviderError {
            code: Some("overloaded".to_owned()),
            message: "The server had an error".to_owned(),
            usage: None,
        }]
    );
    let typed_only = [unnamed(
        json!({ "error": { "message": "boom", "type": "server_error" } }),
    )];
    assert_eq!(
        translate_all(&typed_only),
        vec![ProviderEvent::ProviderError {
            code: Some("server_error".to_owned()),
            message: "boom".to_owned(),
            usage: None,
        }]
    );
}

#[test]
fn malformed_data_and_other_markers_are_ignored() {
    let events = [
        raw("not json"),
        raw(""),
        raw("{\"choices\": 5}"),
        unnamed(json!([1, 2])),
        raw("[DONE]"),
    ];
    assert!(translate_all(&events).is_empty());
}

#[test]
fn the_event_name_plays_no_role() {
    let mut translator = adapter().translator();
    let event = SseEvent {
        event: Some("message".to_owned()),
        data: json!({ "choices": [ { "delta": { "content": "x" } } ] }).to_string(),
        id: None,
    };
    assert_eq!(translator.translate(&event), vec![text_delta("x")]);
}

#[test]
fn only_the_first_choice_is_read() {
    let event = unnamed(json!({ "choices": [
        { "index": 0, "delta": { "content": "a" } },
        { "index": 1, "delta": { "content": "b" } }
    ] }));
    assert_eq!(
        adapter().translator().translate(&event),
        vec![text_delta("a")]
    );
}

#[test]
fn the_adapter_creates_independent_translators() {
    let adapter = adapter();
    let mut first = adapter.translator();
    let mut second = adapter.translator();
    first.translate(&chunk(&json!({}), Some("stop")));
    assert!(second.finish().is_empty(), "the second one holds nothing");
    assert_eq!(first.finish(), vec![completed(None, None)]);
}

#[test]
fn the_request_and_the_history_helpers_agree_with_the_golden_body() {
    // The golden body above is built from the same fixtures as these.
    let mut parts = parts();
    parts.plan.history = vec![history(HistoryRole::User, "q")];
    assert_eq!(
        build(&parts).body["messages"],
        json!([
            { "role": "system", "content": "SYS" },
            { "role": "user", "content": "q" },
            { "role": "user", "content": "hello" }
        ])
    );
    assert_eq!(header(&build(&parts), "accept"), Some("text/event-stream"));
}
