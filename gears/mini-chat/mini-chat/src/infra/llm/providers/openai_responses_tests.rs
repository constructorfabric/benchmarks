#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(
    clippy::redundant_clone,
    clippy::needless_pass_by_value,
    clippy::non_ascii_literal
)]
//! Slice 4: the `openai_responses` adapter: the golden request bodies of the
//! wire contract (`cpt-cf-mini-chat-dod-streaming-turns-openai-responses-adapter`)
//! and the translation of scripted provider events
//! (`cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream`).

use mini_chat_sdk::{ModelCatalogEntry, ModelTier, WebSearchContextSize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::*;
use crate::domain::service::quota::test_support::model;
use crate::domain::service::stream::context::{ContextPlan, CurrentMessage, SummaryItem};
use crate::domain::service::stream::inputs::ImageAttachment;
use crate::domain::service::stream::ports::{HistoryMessage, HistoryRole, ProviderTarget};
use crate::domain::service::stream::tools::{ToolSpec, ToolsDecision};
use crate::domain::service::stream::types::{Annotation, FunctionCall, ToolName};
use crate::test_doubles::LogCapture;

const TENANT: Uuid = Uuid::from_u128(0x7E);
const USER: Uuid = Uuid::from_u128(0x55);
const CHAT: Uuid = Uuid::from_u128(0xC1);

struct Parts {
    target: ProviderTarget,
    entry: ModelCatalogEntry,
    plan: ContextPlan,
    tools: ToolsDecision,
    max_output: i64,
}

impl Parts {
    fn new() -> Self {
        let mut entry = model("p", ModelTier::Premium).build();
        entry.provider_model_id = "gpt-test".to_owned();
        entry.system_prompt = "SYS".to_owned();
        entry.max_tool_calls = 2;
        entry.web_search_context_size = WebSearchContextSize::Medium;
        entry.max_num_results = 7;
        Self {
            target: ProviderTarget {
                kind: ProviderKind::OpenaiResponses,
                alias: "api.openai.test".to_owned(),
                api_path: "/v1/responses".to_owned(),
                api_version: None,
            },
            entry,
            plan: ContextPlan {
                instructions: "SYS".to_owned(),
                current: Some(CurrentMessage {
                    content: "hello".to_owned(),
                    images: Vec::new(),
                }),
                ..ContextPlan::default()
            },
            tools: ToolsDecision {
                feature_label: "none".to_owned(),
                ..ToolsDecision::default()
            },
            max_output: 500,
        }
    }

    fn input(&self) -> ProviderRequestInput<'_> {
        ProviderRequestInput {
            target: &self.target,
            entry: &self.entry,
            plan: &self.plan,
            tools: &self.tools,
            max_output_tokens_applied: self.max_output,
            tenant_id: TENANT,
            user_id: USER,
            chat_id: CHAT,
        }
    }

    fn build(&self) -> BuiltRequest {
        OpenAiResponsesAdapter::new()
            .build_request(&self.input())
            .expect("the request is built")
    }

    fn body(&self) -> Value {
        self.build().body
    }
}

fn user_hex() -> String {
    format!("{}{}", TENANT.simple(), USER.simple())
}

fn metadata(feature: &str) -> Value {
    json!({
        "tenant_id": TENANT.to_string(),
        "user_id": USER.to_string(),
        "chat_id": CHAT.to_string(),
        "request_type": "chat",
        "feature": feature
    })
}

fn history(role: HistoryRole, content: &str) -> HistoryMessage {
    HistoryMessage {
        id: Uuid::from_u128(0x4001),
        role,
        content: content.to_owned(),
        created_at: time::OffsetDateTime::UNIX_EPOCH,
    }
}

fn with_tools(parts: &mut Parts, tools: Vec<ToolSpec>, label: &str) {
    parts.tools = ToolsDecision {
        tools,
        feature_label: label.to_owned(),
        ..ToolsDecision::default()
    };
}

fn file_search() -> ToolSpec {
    ToolSpec::FileSearch {
        vector_store_id: "vs_chat".to_owned(),
        max_num_results: 7,
    }
}

fn web_search() -> ToolSpec {
    ToolSpec::WebSearch {
        search_context_size: WebSearchContextSize::Medium,
    }
}

fn code_interpreter() -> ToolSpec {
    ToolSpec::CodeInterpreter {
        file_ids: vec!["file-sheet1".to_owned()],
    }
}

// ==== the request ================================================================================

#[test]
fn the_minimal_request_is_exactly_this_body() {
    assert_eq!(
        Parts::new().body(),
        json!({
            "model": "gpt-test",
            "instructions": "SYS",
            "input": [
                { "role": "user", "content": [ { "type": "input_text", "text": "hello" } ] }
            ],
            "max_output_tokens": 500,
            "stream": true,
            "store": false,
            "user": user_hex(),
            "metadata": metadata("none")
        })
    );
}

#[test]
fn the_uri_is_the_alias_and_the_api_path_and_the_method_is_post_with_two_headers() {
    let built = Parts::new().build();
    assert_eq!(built.kind, ProviderKind::OpenaiResponses);
    assert_eq!(built.uri, "/api.openai.test/v1/responses");
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
        ],
        "no credential header: OAGW injects it"
    );
}

#[test]
fn the_model_placeholder_and_the_query_of_the_api_path_are_kept() {
    let mut parts = Parts::new();
    parts.target.api_path =
        "/openai/deployments/{model}/responses?api-version=2025-01-01-preview".to_owned();
    assert_eq!(
        parts.build().uri,
        "/api.openai.test/openai/deployments/gpt-test/responses?api-version=2025-01-01-preview"
    );
    parts.target.api_path = "/openai/v1/responses?api-version=2025-01-01-preview".to_owned();
    assert_eq!(
        parts.build().uri,
        "/api.openai.test/openai/v1/responses?api-version=2025-01-01-preview"
    );
}

#[test]
fn the_user_value_is_the_two_simple_uuids_tenant_first() {
    let body = Parts::new().body();
    let user = body["user"].as_str().unwrap();
    assert_eq!(user.len(), 64);
    assert!(user.starts_with(&TENANT.simple().to_string()));
    assert!(user.ends_with(&USER.simple().to_string()));
}

#[test]
fn instructions_are_omitted_when_empty() {
    let mut parts = Parts::new();
    parts.plan.instructions = String::new();
    assert!(parts.body().get("instructions").is_none());
}

#[test]
fn instructions_carry_the_system_prompt_and_the_guards() {
    let mut parts = Parts::new();
    parts.plan.instructions = "SYS\n\nFS-GUARD\n\nWS-GUARD".to_owned();
    assert_eq!(parts.body()["instructions"], "SYS\n\nFS-GUARD\n\nWS-GUARD");
}

#[test]
fn the_input_is_the_summary_the_history_and_the_current_message_in_order() {
    let mut parts = Parts::new();
    parts.plan.summary = Some(SummaryItem {
        text: "PREAMBLE\n\nthe summary".to_owned(),
    });
    parts.plan.history = vec![
        history(HistoryRole::User, "q1"),
        history(HistoryRole::Assistant, "a1"),
    ];
    assert_eq!(
        parts.body()["input"],
        json!([
            { "role": "user", "content": [ { "type": "input_text", "text": "PREAMBLE\n\nthe summary" } ] },
            { "role": "user", "content": [ { "type": "input_text", "text": "q1" } ] },
            { "role": "assistant", "content": [ { "type": "output_text", "text": "a1" } ] },
            { "role": "user", "content": [ { "type": "input_text", "text": "hello" } ] }
        ])
    );
}

#[test]
fn each_image_is_one_input_image_after_the_text_with_its_provider_file_id() {
    let mut parts = Parts::new();
    parts.plan.current = Some(CurrentMessage {
        content: "look".to_owned(),
        images: vec![
            ImageAttachment {
                id: Uuid::from_u128(1),
                provider_file_id: Some("file-img1".to_owned()),
                secondary_file_id: Some("sec-1".to_owned()),
            },
            ImageAttachment {
                id: Uuid::from_u128(2),
                provider_file_id: Some("file-img2".to_owned()),
                secondary_file_id: None,
            },
        ],
    });
    assert_eq!(
        parts.body()["input"][0],
        json!({
            "role": "user",
            "content": [
                { "type": "input_text", "text": "look" },
                { "type": "input_image", "file_id": "file-img1" },
                { "type": "input_image", "file_id": "file-img2" }
            ]
        })
    );
}

#[test]
fn an_image_without_a_provider_file_id_is_dropped_with_a_warning() {
    let logs = LogCapture::start();
    let mut parts = Parts::new();
    parts.plan.current = Some(CurrentMessage {
        content: "look".to_owned(),
        images: vec![ImageAttachment {
            id: Uuid::from_u128(1),
            provider_file_id: None,
            secondary_file_id: None,
        }],
    });
    assert_eq!(
        parts.body()["input"][0]["content"],
        json!([{ "type": "input_text", "text": "look" }])
    );
    assert!(!logs.at(tracing::Level::WARN).is_empty());
}

// ---- tools --------------------------------------------------------------------------------------

#[test]
fn tools_are_sent_in_the_fixed_order_with_their_parameters() {
    let mut parts = Parts::new();
    with_tools(
        &mut parts,
        vec![
            file_search(),
            web_search(),
            code_interpreter(),
            ToolSpec::SearchKnowledge,
        ],
        "file_search+web_search+code_interpreter",
    );
    let body = parts.body();
    let tools = body["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 4);
    assert_eq!(
        tools[0],
        json!({ "type": "file_search", "vector_store_ids": ["vs_chat"], "max_num_results": 7 })
    );
    assert_eq!(
        tools[1],
        json!({ "type": "web_search", "search_context_size": "medium" })
    );
    assert_eq!(
        tools[2],
        json!({ "type": "code_interpreter", "container": { "type": "auto", "file_ids": ["file-sheet1"] } })
    );
    assert_eq!(tools[3]["type"], "function");
    assert_eq!(tools[3]["name"], "search_knowledge");
    assert!(
        tools[3]["description"]
            .as_str()
            .is_some_and(|d| !d.is_empty())
    );
    assert_eq!(tools[3]["parameters"]["type"], "object");
    assert_eq!(tools[3]["parameters"]["required"], json!(["query"]));
    assert_eq!(
        tools[3]["parameters"]["properties"]["query"]["type"],
        "string"
    );
    assert_eq!(
        tools[3]["parameters"]["properties"]["top_k"]["type"],
        "integer"
    );
}

#[test]
fn the_web_search_context_size_comes_from_the_catalog() {
    for (size, wire) in [
        (WebSearchContextSize::Low, "low"),
        (WebSearchContextSize::Medium, "medium"),
        (WebSearchContextSize::High, "high"),
    ] {
        let mut parts = Parts::new();
        with_tools(
            &mut parts,
            vec![ToolSpec::WebSearch {
                search_context_size: size,
            }],
            "web_search",
        );
        assert_eq!(parts.body()["tools"][0]["search_context_size"], wire);
    }
}

#[test]
fn tools_are_absent_when_empty() {
    let body = Parts::new().body();
    assert!(body.get("tools").is_none());
    assert!(body.get("include").is_none());
    assert!(body.get("max_tool_calls").is_none());
}

#[test]
fn include_is_sent_only_with_the_code_interpreter() {
    let mut parts = Parts::new();
    with_tools(&mut parts, vec![code_interpreter()], "code_interpreter");
    assert_eq!(
        parts.body()["include"],
        json!(["code_interpreter_call.outputs"])
    );
    with_tools(&mut parts, vec![web_search()], "web_search");
    assert!(parts.body().get("include").is_none());
}

#[test]
fn max_tool_calls_is_sent_only_with_a_built_in_tool() {
    let mut parts = Parts::new();
    parts.entry.max_tool_calls = 5;
    with_tools(&mut parts, vec![web_search()], "web_search");
    assert_eq!(parts.body()["max_tool_calls"], 5);
    with_tools(&mut parts, vec![file_search()], "file_search");
    assert_eq!(parts.body()["max_tool_calls"], 5);
    with_tools(&mut parts, vec![ToolSpec::SearchKnowledge], "none");
    assert!(
        parts.body().get("max_tool_calls").is_none(),
        "a function tool alone has no built-in tool limit"
    );
    with_tools(&mut parts, vec![], "none");
    assert!(parts.body().get("max_tool_calls").is_none());
}

#[test]
fn the_feature_label_of_the_decision_is_the_metadata_feature() {
    let mut parts = Parts::new();
    with_tools(
        &mut parts,
        vec![file_search(), web_search()],
        "file_search+web_search",
    );
    assert_eq!(parts.body()["metadata"], metadata("file_search+web_search"));
}

#[test]
fn the_output_limit_stream_and_store_are_fixed() {
    let mut parts = Parts::new();
    parts.max_output = 1234;
    let body = parts.body();
    assert_eq!(body["max_output_tokens"], 1234);
    assert_eq!(body["stream"], true);
    assert_eq!(body["store"], false);
}

// ---- parameters and extra_body ------------------------------------------------------------------

#[test]
fn sampling_parameters_are_sent_only_when_set() {
    let mut parts = Parts::new();
    let params = &mut parts.entry.general_config.api_params;
    params.temperature = Some(0.2);
    params.top_p = Some(0.9);
    params.frequency_penalty = Some(0.1);
    params.presence_penalty = Some(0.3);
    params.reasoning_effort = Some("high".to_owned());
    let body = parts.body();
    assert_eq!(body["temperature"], 0.2);
    assert_eq!(body["top_p"], 0.9);
    assert_eq!(body["frequency_penalty"], 0.1);
    assert_eq!(body["presence_penalty"], 0.3);
    assert_eq!(body["reasoning"], json!({ "effort": "high" }));

    let plain = Parts::new().body();
    for key in [
        "temperature",
        "top_p",
        "frequency_penalty",
        "presence_penalty",
        "reasoning",
    ] {
        assert!(plain.get(key).is_none(), "{key}");
    }
}

#[test]
fn stop_sequences_are_never_sent() {
    let mut parts = Parts::new();
    parts.entry.general_config.api_params.stop = vec!["END".to_owned()];
    assert!(parts.body().get("stop").is_none());
}

#[test]
fn extra_body_keys_are_merged_at_the_top_level() {
    let mut parts = Parts::new();
    parts.entry.general_config.api_params.extra_body =
        Some(json!({ "seed": 7, "service_tier": "flex", "parallel_tool_calls": false }));
    let body = parts.body();
    assert_eq!(body["seed"], 7);
    assert_eq!(body["service_tier"], "flex");
    assert_eq!(body["parallel_tool_calls"], false);
}

#[test]
fn controlled_extra_body_keys_are_ignored_with_one_warning_each() {
    let logs = LogCapture::start();
    let mut parts = Parts::new();
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
    let body = parts.body();
    assert_eq!(body, {
        let mut expected = Parts::new().body();
        expected["seed"] = json!(1);
        expected
    });
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
    let mut parts = Parts::new();
    parts.entry.general_config.api_params.extra_body = Some(json!(["a", "b"]));
    assert_eq!(parts.body(), Parts::new().body());
}

#[test]
fn nothing_the_gear_must_never_send_is_ever_in_the_body() {
    let mut parts = Parts::new();
    parts.entry.general_config.tool_support.mcp = true;
    parts.entry.general_config.api_params.stop = vec!["x".to_owned()];
    with_tools(
        &mut parts,
        vec![
            file_search(),
            web_search(),
            code_interpreter(),
            ToolSpec::SearchKnowledge,
        ],
        "file_search+web_search+code_interpreter",
    );
    let text = parts.body().to_string();
    for forbidden in [
        "previous_response_id",
        "tool_choice",
        "\"stop\"",
        "mcp",
        "authorization",
        "api-key",
    ] {
        assert!(!text.contains(forbidden), "`{forbidden}` in {text}");
    }
}

#[test]
fn the_full_request_has_no_key_beyond_the_contract() {
    let mut parts = Parts::new();
    let params = &mut parts.entry.general_config.api_params;
    params.temperature = Some(0.5);
    params.top_p = Some(1.0);
    params.frequency_penalty = Some(0.0);
    params.presence_penalty = Some(0.0);
    params.reasoning_effort = Some("low".to_owned());
    with_tools(
        &mut parts,
        vec![file_search(), web_search(), code_interpreter()],
        "file_search+web_search+code_interpreter",
    );
    let body = parts.body();
    let mut keys: Vec<_> = body.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "frequency_penalty",
            "include",
            "input",
            "instructions",
            "max_output_tokens",
            "max_tool_calls",
            "metadata",
            "model",
            "presence_penalty",
            "reasoning",
            "store",
            "stream",
            "temperature",
            "tools",
            "top_p",
            "user"
        ]
    );
}

#[test]
fn identical_inputs_give_byte_identical_bodies() {
    let mut parts = Parts::new();
    parts.plan.history = vec![
        history(HistoryRole::User, "q"),
        history(HistoryRole::Assistant, "a"),
    ];
    let first = serde_json::to_vec(&parts.body()).unwrap();
    let second = serde_json::to_vec(&parts.body()).unwrap();
    assert_eq!(first, second);
}

#[test]
fn the_adapter_serves_the_openai_responses_kind() {
    assert_eq!(
        OpenAiResponsesAdapter::new().kind(),
        ProviderKind::OpenaiResponses
    );
}

// ==== the translation ============================================================================

fn named(name: &str, data: Value) -> SseEvent {
    SseEvent {
        event: Some(name.to_owned()),
        data: data.to_string(),
        id: None,
    }
}

fn translate(event: &SseEvent) -> Vec<ProviderEvent> {
    ResponsesTranslator::default().translate(event)
}

fn usage(input: u64, output: u64, cached: u64, reasoning: u64) -> mini_chat_sdk::UsageTokens {
    mini_chat_sdk::UsageTokens {
        input_tokens: input,
        output_tokens: output,
        cache_read_input_tokens: cached,
        cache_write_input_tokens: 0,
        reasoning_tokens: reasoning,
    }
}

#[test]
fn a_text_delta_becomes_a_text_delta_event() {
    assert_eq!(
        translate(&named(
            "response.output_text.delta",
            json!({ "type": "response.output_text.delta", "item_id": "msg_1", "content_index": 0, "delta": "Hel" })
        )),
        vec![ProviderEvent::TextDelta {
            text: "Hel".to_owned(),
            item_id: Some("msg_1".to_owned()),
            content_index: Some(0),
        }]
    );
}

#[test]
fn the_event_name_comes_from_the_type_when_the_line_is_missing_or_message() {
    let data = json!({ "type": "response.output_text.delta", "delta": "x" });
    for name in [None, Some("message")] {
        let event = SseEvent {
            event: name.map(str::to_owned),
            data: data.to_string(),
            id: None,
        };
        assert_eq!(
            translate(&event),
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
fn the_event_line_wins_over_the_type_field() {
    let event = SseEvent {
        event: Some("response.created".to_owned()),
        data: json!({ "type": "response.output_text.delta", "delta": "x" }).to_string(),
        id: None,
    };
    assert!(translate(&event).is_empty());
}

#[test]
fn file_search_start_and_done_carry_the_number_of_results() {
    assert_eq!(
        translate(&named("response.file_search_call.searching", json!({}))),
        vec![ProviderEvent::ToolStart {
            name: ToolName::FileSearch,
            details: json!({})
        }]
    );
    assert_eq!(
        translate(&named(
            "response.file_search_call.completed",
            json!({ "results": [ {"file_id": "a"}, {"file_id": "b"}, {"file_id": "c"} ] })
        )),
        vec![ProviderEvent::ToolDone {
            name: ToolName::FileSearch,
            details: json!({ "files_searched": 3 }),
        }]
    );
    assert_eq!(
        translate(&named("response.file_search_call.completed", json!({}))),
        vec![ProviderEvent::ToolDone {
            name: ToolName::FileSearch,
            details: json!({ "files_searched": 0 }),
        }]
    );
}

#[test]
fn web_search_start_and_done_have_empty_details() {
    assert_eq!(
        translate(&named("response.web_search_call.searching", json!({}))),
        vec![ProviderEvent::ToolStart {
            name: ToolName::WebSearch,
            details: json!({})
        }]
    );
    assert_eq!(
        translate(&named("response.web_search_call.completed", json!({}))),
        vec![ProviderEvent::ToolDone {
            name: ToolName::WebSearch,
            details: json!({})
        }]
    );
}

#[test]
fn code_interpreter_in_progress_is_a_start_and_the_other_call_events_are_ignored() {
    assert_eq!(
        translate(&named(
            "response.code_interpreter_call.in_progress",
            json!({})
        )),
        vec![ProviderEvent::ToolStart {
            name: ToolName::CodeInterpreter,
            details: json!({})
        }]
    );
    for name in [
        "response.code_interpreter_call.interpreting",
        "response.code_interpreter_call.completed",
        "response.code_interpreter_call_code.delta",
    ] {
        assert!(translate(&named(name, json!({}))).is_empty(), "{name}");
    }
}

fn code_item(outputs: Value) -> SseEvent {
    named(
        "response.output_item.done",
        json!({ "item": { "type": "code_interpreter_call", "id": "ci_1", "outputs": outputs } }),
    )
}

#[test]
fn a_finished_code_interpreter_item_reports_its_logs_joined_by_newlines() {
    assert_eq!(
        translate(&code_item(json!([
            { "type": "logs", "logs": "first" },
            { "type": "image", "url": "https://example.org/x.png" },
            { "type": "logs", "logs": "second" }
        ]))),
        vec![ProviderEvent::ToolDone {
            name: ToolName::CodeInterpreter,
            details: json!({ "output": "first\nsecond" }),
        }]
    );
}

#[test]
fn a_code_interpreter_item_without_outputs_has_an_empty_output() {
    for outputs in [json!([]), json!(null)] {
        assert_eq!(
            translate(&code_item(outputs)),
            vec![ProviderEvent::ToolDone {
                name: ToolName::CodeInterpreter,
                details: json!({ "output": "" }),
            }]
        );
    }
}

#[test]
fn a_long_code_output_is_cut_to_8192_characters_plus_a_marker() {
    let exact = "a".repeat(8192);
    let events = translate(&code_item(json!([{ "type": "logs", "logs": exact }])));
    assert_eq!(
        events,
        vec![ProviderEvent::ToolDone {
            name: ToolName::CodeInterpreter,
            details: json!({ "output": "a".repeat(8192) }),
        }],
        "exactly the cap is not cut"
    );
    let long = "b".repeat(8193);
    let events = translate(&code_item(json!([{ "type": "logs", "logs": long }])));
    assert_eq!(
        events,
        vec![ProviderEvent::ToolDone {
            name: ToolName::CodeInterpreter,
            details: json!({ "output": format!("{}...[truncated]", "b".repeat(8192)) }),
        }]
    );
}

#[test]
fn the_output_cap_counts_characters_not_bytes() {
    let long = "é".repeat(9000);
    let events = translate(&code_item(json!([{ "type": "logs", "logs": long }])));
    let ProviderEvent::ToolDone { details, .. } = &events[0] else {
        panic!("tool done expected");
    };
    let output = details["output"].as_str().unwrap();
    assert_eq!(
        output.chars().count(),
        8192 + "...[truncated]".chars().count()
    );
    assert!(output.ends_with("...[truncated]"));
}

#[test]
fn other_finished_items_are_ignored() {
    for item in [
        json!({ "type": "message", "content": [] }),
        json!({ "type": "reasoning" }),
        json!({ "type": "web_search_call" }),
        json!({ "type": "file_search_call" }),
    ] {
        assert!(translate(&named("response.output_item.done", json!({ "item": item }))).is_empty());
    }
}

#[test]
fn a_streamed_annotation_is_an_annotation_event_with_its_position() {
    let annotation = json!({ "type": "url_citation", "url": "https://e.org", "title": "T", "start_index": 1, "end_index": 4 });
    assert_eq!(
        translate(&named(
            "response.output_text.annotation.added",
            json!({ "item_id": "msg_1", "content_index": 2, "annotation": annotation.clone() })
        )),
        vec![ProviderEvent::Annotation(Annotation {
            value: annotation,
            item_id: Some("msg_1".to_owned()),
            content_index: Some(2),
            part_text: None,
        })]
    );
}

#[test]
fn completion_maps_the_usage_and_keeps_the_response_id() {
    let events = translate(&named(
        "response.completed",
        json!({ "response": {
            "id": "resp_abc",
            "usage": {
                "input_tokens": 120, "output_tokens": 45,
                "input_tokens_details": { "cached_tokens": 30 },
                "output_tokens_details": { "reasoning_tokens": 12 }
            }
        }}),
    ));
    assert_eq!(
        events,
        vec![ProviderEvent::Completed {
            usage: Some(usage(120, 45, 30, 12)),
            response_id: Some("resp_abc".to_owned()),
            incomplete_reason: None,
        }]
    );
}

#[test]
fn missing_usage_fields_are_zero_and_a_missing_usage_object_is_none() {
    let partial = translate(&named(
        "response.completed",
        json!({ "response": { "id": "r", "usage": { "input_tokens": 5, "input_tokens_details": null } } }),
    ));
    assert_eq!(
        partial,
        vec![ProviderEvent::Completed {
            usage: Some(usage(5, 0, 0, 0)),
            response_id: Some("r".to_owned()),
            incomplete_reason: None,
        }]
    );
    let none = translate(&named(
        "response.completed",
        json!({ "response": { "id": "r" } }),
    ));
    assert_eq!(
        none,
        vec![ProviderEvent::Completed {
            usage: None,
            response_id: Some("r".to_owned()),
            incomplete_reason: None,
        }]
    );
}

fn completed_with_output(output: Value) -> SseEvent {
    named(
        "response.completed",
        json!({ "response": { "id": "r", "usage": { "input_tokens": 1, "output_tokens": 2 }, "output": output } }),
    )
}

#[test]
fn annotations_of_the_completed_payload_come_first_with_their_part_text() {
    let annotation = json!({ "type": "url_citation", "url": "https://e.org", "title": "T", "start_index": 0, "end_index": 5 });
    let events = translate(&completed_with_output(json!([
        { "type": "message", "id": "msg_9", "content": [
            { "type": "output_text", "text": "Hello world", "annotations": [annotation.clone()] },
            { "type": "output_text", "text": "second part", "annotations": [] }
        ]}
    ])));
    assert_eq!(events.len(), 2);
    assert_eq!(
        events[0],
        ProviderEvent::Annotation(Annotation {
            value: annotation,
            item_id: Some("msg_9".to_owned()),
            content_index: Some(0),
            part_text: Some("Hello world".to_owned()),
        })
    );
    assert!(matches!(events[1], ProviderEvent::Completed { .. }));
}

#[test]
fn the_completed_payload_annotations_are_skipped_after_a_streamed_annotation() {
    let mut translator = ResponsesTranslator::default();
    let annotation = json!({ "type": "url_citation", "url": "https://e.org", "title": "T" });
    translator.translate(&named(
        "response.output_text.annotation.added",
        json!({ "item_id": "msg_1", "content_index": 0, "annotation": annotation.clone() }),
    ));
    let events = translator.translate(&completed_with_output(json!([
        { "type": "message", "id": "msg_1", "content": [
            { "type": "output_text", "text": "x", "annotations": [annotation] }
        ]}
    ])));
    assert_eq!(events.len(), 1, "only the completion: {events:?}");
    assert!(matches!(events[0], ProviderEvent::Completed { .. }));
}

#[test]
fn function_calls_in_the_output_make_the_outcome_a_tool_use() {
    let events = translate(&completed_with_output(json!([
        { "type": "function_call", "call_id": "call_1", "name": "search_knowledge", "arguments": "{\"query\":\"q\"}" },
        { "type": "function_call", "call_id": "call_2", "name": "load_files", "arguments": "{}" }
    ])));
    assert_eq!(
        events,
        vec![ProviderEvent::ToolUse {
            calls: vec![
                FunctionCall {
                    call_id: "call_1".to_owned(),
                    name: "search_knowledge".to_owned(),
                    arguments: "{\"query\":\"q\"}".to_owned(),
                },
                FunctionCall {
                    call_id: "call_2".to_owned(),
                    name: "load_files".to_owned(),
                    arguments: "{}".to_owned(),
                },
            ],
            usage: Some(usage(1, 2, 0, 0)),
        }]
    );
}

#[test]
fn an_incomplete_response_completes_with_its_reason_and_usage() {
    let events = translate(&named(
        "response.incomplete",
        json!({ "response": {
            "id": "resp_i",
            "incomplete_details": { "reason": "max_output_tokens" },
            "usage": { "input_tokens": 10, "output_tokens": 20 }
        }}),
    ));
    assert_eq!(
        events,
        vec![ProviderEvent::Completed {
            usage: Some(usage(10, 20, 0, 0)),
            response_id: Some("resp_i".to_owned()),
            incomplete_reason: Some("max_output_tokens".to_owned()),
        }]
    );
}

#[test]
fn an_incomplete_response_without_a_reason_still_has_one() {
    let events = translate(&named(
        "response.incomplete",
        json!({ "response": { "id": "r" } }),
    ));
    let [
        ProviderEvent::Completed {
            incomplete_reason, ..
        },
    ] = events.as_slice()
    else {
        panic!("completed expected: {events:?}");
    };
    assert_eq!(incomplete_reason.as_deref(), Some("unknown"));
}

#[test]
fn a_failed_response_is_a_provider_error_with_the_code_message_and_usage() {
    let events = translate(&named(
        "response.failed",
        json!({ "response": {
            "error": { "code": "server_error", "message": "boom" },
            "usage": { "input_tokens": 3, "output_tokens": 0 }
        }}),
    ));
    assert_eq!(
        events,
        vec![ProviderEvent::ProviderError {
            code: Some("server_error".to_owned()),
            message: "boom".to_owned(),
            usage: Some(usage(3, 0, 0, 0)),
        }]
    );
}

#[test]
fn a_failed_response_falls_back_to_a_top_level_error() {
    let events = translate(&named(
        "response.failed",
        json!({ "error": { "code": "rate_limit", "message": "slow down" } }),
    ));
    assert_eq!(
        events,
        vec![ProviderEvent::ProviderError {
            code: Some("rate_limit".to_owned()),
            message: "slow down".to_owned(),
            usage: None,
        }]
    );
}

#[test]
fn a_failed_response_without_any_error_has_an_empty_message() {
    let events = translate(&named(
        "response.failed",
        json!({ "response": { "id": "r" } }),
    ));
    assert_eq!(
        events,
        vec![ProviderEvent::ProviderError {
            code: None,
            message: String::new(),
            usage: None
        }]
    );
}

#[test]
fn an_error_event_is_parsed_like_a_failed_response_then_as_a_flat_object() {
    assert_eq!(
        translate(&named(
            "error",
            json!({ "response": { "error": { "code": "c", "message": "nested" } } })
        )),
        vec![ProviderEvent::ProviderError {
            code: Some("c".to_owned()),
            message: "nested".to_owned(),
            usage: None,
        }]
    );
    assert_eq!(
        translate(&named(
            "error",
            json!({ "error": { "code": "c2", "message": "top" } })
        )),
        vec![ProviderEvent::ProviderError {
            code: Some("c2".to_owned()),
            message: "top".to_owned(),
            usage: None,
        }]
    );
    assert_eq!(
        translate(&named("error", json!({ "code": "c3", "message": "flat" }))),
        vec![ProviderEvent::ProviderError {
            code: Some("c3".to_owned()),
            message: "flat".to_owned(),
            usage: None,
        }]
    );
}

#[test]
fn an_error_event_whose_data_does_not_parse_uses_the_data_as_the_message() {
    let event = SseEvent {
        event: Some("error".to_owned()),
        data: "upstream connect error".to_owned(),
        id: None,
    };
    assert_eq!(
        translate(&event),
        vec![ProviderEvent::ProviderError {
            code: None,
            message: "upstream connect error".to_owned(),
            usage: None,
        }]
    );
}

#[test]
fn every_other_event_is_ignored() {
    for name in [
        "response.created",
        "response.in_progress",
        "response.queued",
        "response.output_item.added",
        "response.content_part.added",
        "response.content_part.done",
        "response.output_text.done",
        "response.reasoning_summary_text.delta",
        "response.function_call_arguments.delta",
        "response.something.new",
        "ping",
    ] {
        assert!(
            translate(&named(name, json!({ "type": name }))).is_empty(),
            "{name}"
        );
    }
}

#[test]
fn malformed_json_of_a_known_event_is_ignored() {
    for name in [
        "response.output_text.delta",
        "response.completed",
        "response.incomplete",
        "response.file_search_call.completed",
        "response.output_item.done",
    ] {
        let event = SseEvent {
            event: Some(name.to_owned()),
            data: "{not json".to_owned(),
            id: None,
        };
        assert!(translate(&event).is_empty(), "{name}");
    }
}

#[test]
fn a_done_marker_and_an_event_without_a_name_or_type_are_ignored() {
    let done = SseEvent {
        event: None,
        data: "[DONE]".to_owned(),
        id: None,
    };
    assert!(translate(&done).is_empty());
    let nameless = SseEvent {
        event: None,
        data: json!({ "no": "type" }).to_string(),
        id: None,
    };
    assert!(translate(&nameless).is_empty());
}

#[test]
fn a_text_delta_without_text_is_ignored() {
    assert!(
        translate(&named(
            "response.output_text.delta",
            json!({ "item_id": "m" })
        ))
        .is_empty()
    );
}

#[test]
fn nothing_is_pending_at_the_end_of_the_body() {
    assert!(ResponsesTranslator::default().finish().is_empty());
}

#[test]
fn the_adapter_creates_independent_translators() {
    let adapter = OpenAiResponsesAdapter::new();
    let mut first = adapter.translator();
    first.translate(&named(
        "response.output_text.annotation.added",
        json!({ "annotation": { "type": "url_citation" } }),
    ));
    // A second stream starts without the annotation state of the first.
    let mut second = adapter.translator();
    let events = second.translate(&completed_with_output(json!([
        { "type": "message", "id": "m", "content": [
            { "type": "output_text", "text": "t", "annotations": [ { "type": "url_citation" } ] }
        ]}
    ])));
    assert_eq!(events.len(), 2);
}

// ==== slice 7: the event name and the continuation ===============================================

#[test]
fn an_empty_event_name_is_treated_as_message_and_the_type_field_names_the_event() {
    let event = SseEvent {
        event: Some(String::new()),
        data: json!({ "type": "response.output_text.delta", "delta": "x" }).to_string(),
        id: None,
    };
    assert_eq!(
        translate(&event),
        vec![ProviderEvent::TextDelta {
            text: "x".to_owned(),
            item_id: None,
            content_index: None,
        }]
    );
}

#[test]
fn an_empty_event_name_without_a_type_is_ignored() {
    let event = SseEvent {
        event: Some(String::new()),
        data: json!({ "delta": "x" }).to_string(),
        id: None,
    };
    assert!(translate(&event).is_empty());
}

fn tool_outputs() -> Vec<crate::domain::service::stream::ports::ToolCallOutput> {
    use crate::domain::service::stream::ports::ToolCallOutput;
    vec![
        ToolCallOutput {
            call: FunctionCall {
                call_id: "call_1".to_owned(),
                name: "search_knowledge".to_owned(),
                arguments: "{\"query\":\"alpha\"}".to_owned(),
            },
            output: "{\"results\":[{\"text\":\"A\"}]}".to_owned(),
        },
        ToolCallOutput {
            call: FunctionCall {
                call_id: "call_2".to_owned(),
                name: "search_knowledge".to_owned(),
                arguments: "{\"query\":\"beta\"}".to_owned(),
            },
            output: "{\"error\":\"search limit reached\"}".to_owned(),
        },
    ]
}

#[test]
fn the_continuation_appends_a_function_call_and_its_output_per_call_to_the_input() {
    let mut parts = Parts::new();
    with_tools(&mut parts, vec![ToolSpec::SearchKnowledge], "none");
    let previous = parts.build();
    let next = OpenAiResponsesAdapter::new()
        .continue_request(&previous, &tool_outputs())
        .unwrap();
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
    assert_eq!(next.body, expected, "the same tools, store and user");
    assert_eq!(next.kind, ProviderKind::OpenaiResponses);
    assert_eq!(next.uri, previous.uri);
    assert_eq!(next.headers, previous.headers);
}

#[test]
fn a_second_continuation_appends_after_the_first_and_leaves_the_previous_request_alone() {
    let previous = Parts::new().build();
    let before = previous.clone();
    let adapter = OpenAiResponsesAdapter::new();
    let first = adapter
        .continue_request(&previous, &tool_outputs())
        .unwrap();
    let second = adapter
        .continue_request(&first, &tool_outputs()[..1])
        .unwrap();
    assert_eq!(previous, before);
    assert_eq!(second.body["input"].as_array().unwrap().len(), 1 + 4 + 2);
}

#[test]
fn a_body_without_an_input_array_cannot_be_continued() {
    let broken = BuiltRequest {
        kind: ProviderKind::OpenaiResponses,
        uri: "/a/b".to_owned(),
        headers: Vec::new(),
        body: json!({ "model": "m" }),
    };
    let error = OpenAiResponsesAdapter::new()
        .continue_request(&broken, &tool_outputs())
        .unwrap_err();
    assert!(matches!(error, DomainError::Internal(_)), "{error:?}");
}
