//! The provider request of the four adapter kinds, against plans built without a database:
//! the URI through OAGW, the `OpenAI` Responses body field by field, and the per-kind request
//! fields, dropped tools and merged `extra_body` of Chat Completions, `vLLM` and Anthropic.
//!
//! Slice S2-provider-path of `cpt-cf-mini-chat-feature-message-streaming`: algorithms
//! `provider-call` (URI), `responses-request`, `adapter-kinds`, `tool-assembly`
//! (`inst-tools-max-calls`) and `configuration`; definition of done
//! `cpt-cf-mini-chat-dod-message-streaming-provider-adapters`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;

use serde_json::{Value, json};
use uuid::Uuid;

use super::context::{ContextRole, THREAD_SUMMARY_PREAMBLE};
use super::provider_request::{build_provider_request, composite_user, provider_uri};
use crate::config::ProviderKind;
use crate::test_support::send::{SYSTEM_PROMPT, WEB_GUARD};
use crate::test_support::streaming::{
    PLAN_TENANT, PLAN_USER, PlanBuilder, code_interpreter_contribution, file_search_contribution,
};

fn body_of(builder: PlanBuilder) -> Value {
    build_provider_request(&builder.build())
        .expect("the request builds")
        .body
}

fn keys(body: &Value) -> BTreeSet<&str> {
    body.as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect()
}

fn set(names: &[&'static str]) -> BTreeSet<&'static str> {
    names.iter().copied().collect()
}

// ── the URI ──────────────────────────────────────────────────────────────────

#[test]
fn the_uri_is_the_alias_and_the_api_path() {
    let plan = PlanBuilder::new().build();
    let request = build_provider_request(&plan).unwrap();
    assert_eq!(request.uri, "/azure.example.com/v1/responses");
    assert_eq!(
        provider_uri(&plan.provider, "provider-standard-1"),
        "/azure.example.com/v1/responses"
    );
}

#[test]
fn a_host_port_alias_is_kept_in_the_uri() {
    let plan = PlanBuilder::new().alias("127.0.0.1:8081").build();
    assert_eq!(
        build_provider_request(&plan).unwrap().uri,
        "/127.0.0.1:8081/v1/responses"
    );
}

#[test]
fn the_model_placeholder_is_replaced_and_the_api_version_query_is_kept_verbatim() {
    let plan = PlanBuilder::new()
        .api_path("/openai/deployments/{model}/responses?api-version=2025-03-01-preview")
        .build();
    assert_eq!(
        build_provider_request(&plan).unwrap().uri,
        "/azure.example.com/openai/deployments/provider-standard-1/responses?api-version=2025-03-01-preview"
    );
}

#[test]
fn the_uri_uses_the_provider_model_id_not_the_internal_model_id() {
    let plan = PlanBuilder::new().api_path("/m/{model}").build();
    let uri = build_provider_request(&plan).unwrap().uri;
    assert!(uri.ends_with("/m/provider-standard-1"), "{uri}");
    assert!(!uri.contains("standard-1/") || uri.contains("provider-standard-1"));
}

// ── the composite user ───────────────────────────────────────────────────────

#[test]
fn the_composite_user_is_64_lowercase_hex_characters_tenant_first() {
    let tenant = Uuid::parse_str("AABBCCDD-0000-4000-8000-0123456789AB").unwrap();
    let user = Uuid::parse_str("11223344-5566-4777-8888-99AABBCCDDEE").unwrap();
    let value = composite_user(tenant, user);
    assert_eq!(value.len(), 64);
    assert_eq!(
        value,
        "aabbccdd0000400080000123456789ab1122334455664777888899aabbccddee"
    );
    assert!(
        value
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    );
    assert!(!value.contains('-'));
}

// ── OpenAI Responses ─────────────────────────────────────────────────────────

#[test]
fn the_responses_body_carries_exactly_the_specified_fields() {
    let plan = PlanBuilder::new().build();
    let body = build_provider_request(&plan).unwrap().body;
    assert_eq!(
        keys(&body),
        set(&[
            "model",
            "instructions",
            "input",
            "stream",
            "max_output_tokens",
            "max_tool_calls",
            "user",
            "metadata",
            "store",
        ]),
        "{body}"
    );
    assert_eq!(body["model"], "provider-standard-1");
    assert_eq!(body["instructions"], SYSTEM_PROMPT);
    assert_eq!(body["stream"], true);
    assert_eq!(body["store"], false);
    assert_eq!(body["max_output_tokens"], 500);
    // the catalog default `max_tool_calls` is 2
    assert_eq!(body["max_tool_calls"], 2);
    assert_eq!(
        body["input"],
        json!([{ "role": "user", "content": "hello" }])
    );
}

#[test]
fn the_catalog_max_tool_calls_and_the_applied_output_cap_are_sent() {
    let body = body_of(
        PlanBuilder::new()
            .model(&json!({ "max_tool_calls": 5 }))
            .max_output_tokens(321),
    );
    assert_eq!(body["max_tool_calls"], 5);
    assert_eq!(body["max_output_tokens"], 321);
}

#[test]
fn the_user_is_the_64_character_hex_of_tenant_then_user() {
    let body = body_of(PlanBuilder::new());
    let user = body["user"].as_str().unwrap();
    assert_eq!(user.len(), 64);
    assert_eq!(
        user,
        format!("{}{}", PLAN_TENANT.simple(), PLAN_USER.simple())
    );
}

#[test]
fn the_metadata_names_the_identity_the_chat_the_request_type_and_the_feature() {
    let plan = PlanBuilder::new().build();
    let body = build_provider_request(&plan).unwrap().body;
    assert_eq!(
        body["metadata"],
        json!({
            "tenant_id": plan.tenant_id.to_string(),
            "user_id": plan.user_id.to_string(),
            "chat_id": plan.chat_id.to_string(),
            "request_type": "chat",
            "feature": "none",
        })
    );
}

#[test]
fn no_previous_response_tool_choice_or_credential_is_ever_sent() {
    let body = body_of(PlanBuilder::new().web_search(true));
    for absent in [
        "previous_response_id",
        "tool_choice",
        "messages",
        "system",
        "stream_options",
        "max_tokens",
        "max_completion_tokens",
        "api_key",
        "authorization",
    ] {
        assert!(body.get(absent).is_none(), "`{absent}` in {body}");
    }
    assert!(!body.to_string().to_lowercase().contains("bearer"));
}

#[test]
fn web_search_adds_the_tool_the_guard_and_the_feature() {
    let body = body_of(
        PlanBuilder::new()
            .model(&json!({ "web_search_context_size": "high" }))
            .web_search(true),
    );
    assert_eq!(
        body["tools"],
        json!([{ "type": "web_search", "search_context_size": "high" }])
    );
    let instructions = body["instructions"].as_str().unwrap();
    assert!(instructions.starts_with(SYSTEM_PROMPT), "{instructions}");
    assert!(instructions.ends_with(WEB_GUARD), "{instructions}");
    assert_eq!(body["metadata"]["feature"], "web_search");
    assert!(body.get("include").is_none());
}

#[test]
fn each_search_context_size_of_the_catalog_is_sent_as_is() {
    for size in ["low", "medium", "high"] {
        let body = body_of(
            PlanBuilder::new()
                .model(&json!({ "web_search_context_size": size }))
                .web_search(true),
        );
        assert_eq!(body["tools"][0]["search_context_size"], size);
    }
}

#[test]
fn a_model_without_web_search_support_sends_no_tool_and_no_guard() {
    let body = body_of(PlanBuilder::new().web_search(false));
    assert!(body.get("tools").is_none(), "{body}");
    assert_eq!(body["instructions"], SYSTEM_PROMPT);
    assert_eq!(body["metadata"]["feature"], "none");
}

#[test]
fn the_code_interpreter_adds_its_tool_and_the_include_entry_only_then() {
    let with = body_of(PlanBuilder::new().contribution(code_interpreter_contribution()));
    assert_eq!(with["tools"][0]["type"], "code_interpreter");
    assert_eq!(with["tools"][0]["container"]["type"], "auto");
    assert_eq!(with["include"], json!(["code_interpreter_call.outputs"]));
    assert_eq!(with["metadata"]["feature"], "code_interpreter");

    let without = body_of(PlanBuilder::new().web_search(true));
    assert!(without.get("include").is_none(), "{without}");
}

#[test]
fn the_tools_follow_the_order_file_search_web_search_code_interpreter() {
    let body = body_of(
        PlanBuilder::new()
            .contribution(code_interpreter_contribution())
            .contribution(file_search_contribution())
            .web_search(true),
    );
    let types: Vec<&str> = body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["type"].as_str().unwrap())
        .collect();
    assert_eq!(types, vec!["file_search", "web_search", "code_interpreter"]);
    assert_eq!(
        body["metadata"]["feature"],
        "file_search+web_search+code_interpreter"
    );
}

#[test]
fn the_input_holds_the_summary_the_history_and_the_current_message_in_plan_order() {
    let body = body_of(
        PlanBuilder::new()
            .summary("the earlier talk")
            .history(ContextRole::User, "q1")
            .history(ContextRole::Assistant, "a1")
            .content("q2"),
    );
    let input = body["input"].as_array().unwrap();
    assert_eq!(input.len(), 4, "{body}");
    assert_eq!(input[0]["role"], "user");
    let summary = input[0]["content"].as_str().unwrap();
    assert!(summary.starts_with(THREAD_SUMMARY_PREAMBLE), "{summary}");
    assert!(summary.ends_with("the earlier talk"), "{summary}");
    assert_eq!(input[1], json!({ "role": "user", "content": "q1" }));
    assert_eq!(input[2], json!({ "role": "assistant", "content": "a1" }));
    assert_eq!(input[3], json!({ "role": "user", "content": "q2" }));
    // the system instructions are never an input item
    assert!(input.iter().all(|item| item["role"] != "system"));
}

#[test]
fn history_items_are_plain_strings_and_a_turn_without_images_has_a_plain_current_string() {
    let body = body_of(
        PlanBuilder::new()
            .history(ContextRole::User, "q1")
            .history(ContextRole::Assistant, "a1"),
    );
    for item in body["input"].as_array().unwrap() {
        assert!(item["content"].is_string(), "{item}");
    }
}

#[test]
fn current_images_follow_the_text_as_input_image_items_in_attachment_order() {
    let body = body_of(
        PlanBuilder::new()
            .content("what is this?")
            .image("file-img-one")
            .image("file-img-two"),
    );
    let input = body["input"].as_array().unwrap();
    assert_eq!(input.len(), 1);
    assert_eq!(
        input[0],
        json!({
            "role": "user",
            "content": [
                { "type": "input_text", "text": "what is this?" },
                { "type": "input_image", "file_id": "file-img-one" },
                { "type": "input_image", "file_id": "file-img-two" },
            ]
        })
    );
}

#[test]
fn history_never_carries_images() {
    let body = body_of(
        PlanBuilder::new()
            .history(ContextRole::User, "earlier")
            .image("file-img-one"),
    );
    let input = body["input"].as_array().unwrap();
    assert_eq!(input[0], json!({ "role": "user", "content": "earlier" }));
    assert!(input[1]["content"].is_array());
}

#[test]
fn api_params_that_are_set_are_sent_and_unset_ones_are_omitted() {
    let unset = body_of(PlanBuilder::new());
    for absent in [
        "temperature",
        "top_p",
        "frequency_penalty",
        "presence_penalty",
        "stop",
        "reasoning",
    ] {
        assert!(unset.get(absent).is_none(), "`{absent}` in {unset}");
    }
    let set_all = body_of(PlanBuilder::new().api_params(&json!({
        "temperature": 0.25,
        "top_p": 0.5,
        "frequency_penalty": 0.5,
        "presence_penalty": 0.75,
        "stop": ["END"],
        "reasoning_effort": "low",
    })));
    assert_eq!(set_all["temperature"], 0.25);
    assert_eq!(set_all["top_p"], 0.5);
    assert_eq!(set_all["frequency_penalty"], 0.5);
    assert_eq!(set_all["presence_penalty"], 0.75);
    assert_eq!(set_all["stop"], json!(["END"]));
    assert_eq!(set_all["reasoning"], json!({ "effort": "low" }));
}

#[test]
fn extra_body_is_merged_except_the_request_controlled_keys() {
    let body = body_of(PlanBuilder::new().api_params(&json!({
        "stop": [],
        "extra_body": {
            "seed": 7,
            "service_tier": "flex",
            "model": "evil",
            "input": [],
            "messages": [],
            "instructions": "evil",
            "system": "evil",
            "stream": false,
            "stream_options": { "include_usage": false },
            "max_output_tokens": 1,
            "max_completion_tokens": 1,
            "max_tokens": 1,
            "max_tool_calls": 99,
            "tools": [{ "type": "evil" }],
            "tool_choice": "none",
            "include": ["evil"],
            "store": true,
            "previous_response_id": "resp_evil",
            "user": "evil",
            "metadata": { "evil": true },
        }
    })));
    assert_eq!(body["seed"], 7);
    assert_eq!(body["service_tier"], "flex");
    assert_eq!(body["model"], "provider-standard-1");
    assert_eq!(body["instructions"], SYSTEM_PROMPT);
    assert_eq!(body["stream"], true);
    assert_eq!(body["store"], false);
    assert_eq!(body["max_output_tokens"], 500);
    assert_eq!(body["max_tool_calls"], 2);
    assert_eq!(
        body["input"],
        json!([{ "role": "user", "content": "hello" }])
    );
    assert_eq!(body["user"].as_str().unwrap().len(), 64);
    assert_eq!(body["metadata"]["request_type"], "chat");
    for absent in [
        "messages",
        "system",
        "stream_options",
        "max_completion_tokens",
        "max_tokens",
        "tools",
        "tool_choice",
        "include",
        "previous_response_id",
    ] {
        assert!(body.get(absent).is_none(), "`{absent}` in {body}");
    }
}

#[test]
fn a_provider_secret_never_appears_in_the_body() {
    let body = body_of(PlanBuilder::new().web_search(true).content("hi"));
    let text = body.to_string();
    for secret in ["sk-", "api-key", "secret_ref", "client_secret"] {
        assert!(!text.contains(secret), "`{secret}` in {text}");
    }
}

// ── Chat Completions ─────────────────────────────────────────────────────────

#[test]
fn chat_completions_sends_messages_with_a_system_message_and_the_usage_request() {
    let body = body_of(
        PlanBuilder::new()
            .kind(ProviderKind::OpenaiChatCompletions)
            .summary("older")
            .history(ContextRole::User, "q1")
            .history(ContextRole::Assistant, "a1")
            .content("q2"),
    );
    assert_eq!(
        keys(&body),
        set(&[
            "model",
            "messages",
            "stream",
            "stream_options",
            "max_completion_tokens",
            "user",
        ]),
        "{body}"
    );
    assert_eq!(body["model"], "provider-standard-1");
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"], json!({ "include_usage": true }));
    assert_eq!(body["max_completion_tokens"], 500);
    assert_eq!(body["user"].as_str().unwrap().len(), 64);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(
        messages[0],
        json!({ "role": "system", "content": SYSTEM_PROMPT })
    );
    assert_eq!(messages[1]["role"], "user");
    assert!(
        messages[1]["content"]
            .as_str()
            .unwrap()
            .starts_with(THREAD_SUMMARY_PREAMBLE)
    );
    assert_eq!(messages[2], json!({ "role": "user", "content": "q1" }));
    assert_eq!(messages[3], json!({ "role": "assistant", "content": "a1" }));
    assert_eq!(messages[4]["role"], "user");
    let last = &messages[4]["content"];
    // a plain string or a single text part
    assert!(
        last == "q2" || last == &json!([{ "type": "text", "text": "q2" }]),
        "{last}"
    );
}

#[test]
fn chat_completions_drops_the_built_in_tools_but_keeps_the_guard() {
    let body = body_of(
        PlanBuilder::new()
            .kind(ProviderKind::OpenaiChatCompletions)
            .contribution(code_interpreter_contribution())
            .contribution(file_search_contribution())
            .web_search(true),
    );
    for absent in ["tools", "include", "metadata", "max_tool_calls", "store"] {
        assert!(body.get(absent).is_none(), "`{absent}` in {body}");
    }
    let system = body["messages"][0]["content"].as_str().unwrap();
    assert!(system.contains(WEB_GUARD), "{system}");
}

#[test]
fn chat_completions_merges_extra_body_without_the_request_controlled_keys() {
    let body = body_of(
        PlanBuilder::new()
            .kind(ProviderKind::OpenaiChatCompletions)
            .api_params(&json!({
                "stop": [],
                "extra_body": { "logit_bias": {}, "max_tokens": 9, "stream_options": {},
                                "model": "evil", "tools": [] }
            })),
    );
    assert_eq!(body["logit_bias"], json!({}));
    assert!(body.get("max_tokens").is_none());
    assert_eq!(body["stream_options"], json!({ "include_usage": true }));
    assert_eq!(body["model"], "provider-standard-1");
    assert!(body.get("tools").is_none());
}

// ── vLLM Responses ───────────────────────────────────────────────────────────

#[test]
fn vllm_drops_every_tool_the_metadata_the_tool_call_bound_and_the_include() {
    let body = body_of(
        PlanBuilder::new()
            .kind(ProviderKind::VllmResponses)
            .contribution(code_interpreter_contribution())
            .contribution(file_search_contribution())
            .web_search(true),
    );
    assert_eq!(
        keys(&body),
        set(&[
            "model",
            "instructions",
            "input",
            "stream",
            "max_output_tokens",
            "user",
            "store",
        ]),
        "{body}"
    );
    // the guards stay in the instructions
    let instructions = body["instructions"].as_str().unwrap();
    assert!(instructions.contains(WEB_GUARD), "{instructions}");
    assert_eq!(body["user"].as_str().unwrap().len(), 64);
    assert_eq!(body["stream"], true);
}

// ── Anthropic Messages ───────────────────────────────────────────────────────

#[test]
fn anthropic_sends_system_messages_max_tokens_and_the_user_as_metadata() {
    let plan = PlanBuilder::new()
        .kind(ProviderKind::AnthropicMessages)
        .summary("older")
        .history(ContextRole::User, "q1")
        .history(ContextRole::Assistant, "a1")
        .content("q2")
        .build();
    let body = build_provider_request(&plan).unwrap().body;
    assert_eq!(
        keys(&body),
        set(&[
            "model",
            "system",
            "messages",
            "max_tokens",
            "stream",
            "metadata"
        ]),
        "{body}"
    );
    assert_eq!(body["model"], "provider-standard-1");
    assert_eq!(body["system"], SYSTEM_PROMPT);
    assert_eq!(body["max_tokens"], 500);
    assert_eq!(body["stream"], true);
    assert_eq!(
        body["metadata"],
        json!({ "user_id": composite_user(plan.tenant_id, plan.user_id) })
    );
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 4, "{body}");
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[2], json!({ "role": "assistant", "content": "a1" }));
    assert_eq!(messages[3]["role"], "user");
}

#[test]
fn anthropic_references_images_by_the_secondary_anthropic_file_id() {
    let body = body_of(
        PlanBuilder::new()
            .kind(ProviderKind::AnthropicMessages)
            .content("what is this?")
            .image_with_secondary("file-x", "file_011CNha8iCJcU1wXNR6q4V8w"),
    );
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(
        messages[0],
        json!({
            "role": "user",
            "content": [
                { "type": "text", "text": "what is this?" },
                { "type": "image",
                  "source": { "type": "file", "file_id": "file_011CNha8iCJcU1wXNR6q4V8w" } },
            ]
        })
    );
    assert!(!body.to_string().contains("file-x"), "{body}");
}

#[test]
fn openai_adapters_keep_the_primary_provider_file_id_of_an_image() {
    let body =
        body_of(PlanBuilder::new().image_with_secondary("file-x", "file_011CNha8iCJcU1wXNR6q4V8w"));
    let content = &body["input"][0]["content"];
    assert_eq!(
        content[1],
        json!({ "type": "input_image", "file_id": "file-x" })
    );
    assert!(!body.to_string().contains("file_011"), "{body}");
}

#[test]
fn anthropic_maps_web_search_and_code_interpreter_to_server_tools_and_drops_file_search() {
    let body = body_of(
        PlanBuilder::new()
            .kind(ProviderKind::AnthropicMessages)
            .contribution(code_interpreter_contribution())
            .contribution(file_search_contribution())
            .web_search(true),
    );
    let tools = body["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 2, "{body}");
    let by_name = |name: &str| {
        tools
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap_or_else(|| panic!("no `{name}` tool in {tools:?}"))
    };
    assert!(
        by_name("web_search")["type"]
            .as_str()
            .unwrap()
            .starts_with("web_search")
    );
    assert!(
        by_name("code_execution")["type"]
            .as_str()
            .unwrap()
            .starts_with("code_execution")
    );
    assert!(!body["tools"].to_string().contains("file_search"));
    // the guards stay in the system text
    assert!(body["system"].as_str().unwrap().contains(WEB_GUARD));
    for absent in [
        "include",
        "max_tool_calls",
        "store",
        "input",
        "instructions",
        "user",
    ] {
        assert!(body.get(absent).is_none(), "`{absent}` in {body}");
    }
}

#[test]
fn anthropic_ignores_extra_body_entirely() {
    let body = body_of(
        PlanBuilder::new()
            .kind(ProviderKind::AnthropicMessages)
            .api_params(&json!({ "stop": [], "extra_body": { "seed": 7, "top_k": 3 } })),
    );
    assert!(body.get("seed").is_none(), "{body}");
    assert!(body.get("top_k").is_none(), "{body}");
}

#[test]
fn every_kind_sends_the_composite_user() {
    for kind in [
        ProviderKind::OpenaiResponses,
        ProviderKind::OpenaiChatCompletions,
        ProviderKind::VllmResponses,
    ] {
        let body = body_of(PlanBuilder::new().kind(kind));
        assert_eq!(
            body["user"],
            format!("{}{}", PLAN_TENANT.simple(), PLAN_USER.simple()),
            "{kind:?}"
        );
    }
}
