//! The bound on built-in tool calls per provider request: only the catalog `max_tool_calls`
//! (default 2), one value shared by every built-in tool, sent by the `OpenAI` Responses adapter and
//! by no other adapter, and no separate per-turn `file_search` limit anywhere in the request or in
//! the mid-turn accounting.
//!
//! Slice S3-completion of `cpt-cf-mini-chat-feature-message-streaming`: algorithm
//! `cpt-cf-mini-chat-algo-message-streaming-tool-assembly` (`inst-tools-max-calls`); definitions of
//! done `cpt-cf-mini-chat-dod-message-streaming-provider-adapters` and
//! `cpt-cf-mini-chat-dod-message-streaming-deferred-scope` (no per-turn `file_search` limit).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use serde_json::{Value, json};

use super::provider_request::build_provider_request;
use super::tool_accounting::{StartDecision, ToolAccounting, ToolLimits};
use crate::config::ProviderKind;
use crate::test_support::streaming::{
    PlanBuilder, code_interpreter_contribution, file_search_contribution,
};

fn body_of(builder: PlanBuilder) -> Value {
    build_provider_request(&builder.build())
        .expect("the request builds")
        .body
}

/// Every key name of a JSON value, at any depth.
fn all_keys(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, inner) in map {
                out.push(key.clone());
                all_keys(inner, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                all_keys(item, out);
            }
        }
        _ => {}
    }
}

fn every_builtin(builder: PlanBuilder) -> PlanBuilder {
    builder
        .contribution(file_search_contribution())
        .contribution(code_interpreter_contribution())
        .web_search(true)
}

#[test]
fn the_responses_adapter_sends_the_catalog_default_of_two_when_the_catalog_sets_none() {
    let body = body_of(PlanBuilder::new());
    assert_eq!(body["max_tool_calls"], 2, "{body}");
}

#[test]
fn the_responses_adapter_sends_the_catalog_value_once_for_all_built_in_tools() {
    for catalog_value in [1_u32, 2, 5, 40] {
        let body = body_of(every_builtin(
            PlanBuilder::new().model(&json!({ "max_tool_calls": catalog_value })),
        ));
        assert_eq!(body["max_tool_calls"], catalog_value, "{body}");
        let types: Vec<&str> = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["type"].as_str().unwrap())
            .collect();
        assert_eq!(
            types,
            vec!["file_search", "web_search", "code_interpreter"],
            "the one bound covers every built-in tool"
        );
        // a single request-level bound: it is not repeated on any tool
        let mut keys = Vec::new();
        all_keys(&body, &mut keys);
        assert_eq!(
            keys.iter().filter(|key| *key == "max_tool_calls").count(),
            1,
            "{body}"
        );
        for tool in body["tools"].as_array().unwrap() {
            assert!(tool.get("max_tool_calls").is_none(), "{tool}");
        }
    }
}

#[test]
fn the_bound_does_not_depend_on_which_tools_the_turn_carries() {
    let without_tools = body_of(PlanBuilder::new().model(&json!({ "max_tool_calls": 7 })));
    let with_tools = body_of(every_builtin(
        PlanBuilder::new().model(&json!({ "max_tool_calls": 7 })),
    ));
    assert_eq!(without_tools["max_tool_calls"], 7);
    assert_eq!(with_tools["max_tool_calls"], 7);
}

#[test]
fn no_other_adapter_sends_a_tool_call_bound() {
    for kind in [
        ProviderKind::OpenaiChatCompletions,
        ProviderKind::VllmResponses,
        ProviderKind::AnthropicMessages,
    ] {
        let body = body_of(every_builtin(
            PlanBuilder::new()
                .kind(kind)
                .model(&json!({ "max_tool_calls": 7 })),
        ));
        let mut keys = Vec::new();
        all_keys(&body, &mut keys);
        for forbidden in [
            "max_tool_calls",
            "max_calls",
            "tool_call_limit",
            "parallel_tool_calls",
        ] {
            assert!(
                !keys.iter().any(|key| key == forbidden),
                "{kind:?} sends `{forbidden}`: {body}"
            );
        }
    }
}

#[test]
fn a_model_extra_body_cannot_override_the_catalog_bound() {
    let body = body_of(
        PlanBuilder::new()
            .model(&json!({ "max_tool_calls": 3 }))
            .api_params(&json!({ "stop": [], "extra_body": { "max_tool_calls": 99 } })),
    );
    assert_eq!(body["max_tool_calls"], 3, "{body}");
}

#[test]
fn the_file_search_tool_carries_no_per_turn_call_limit() {
    let body = body_of(every_builtin(PlanBuilder::new()));
    let file_search = body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["type"] == "file_search")
        .expect("the file_search tool");
    let mut keys = Vec::new();
    all_keys(file_search, &mut keys);
    for forbidden in [
        "max_calls",
        "max_tool_calls",
        "max_per_turn",
        "call_limit",
        "limit",
    ] {
        assert!(
            !keys.iter().any(|key| key == forbidden),
            "a per-turn limit `{forbidden}` on file_search: {file_search}"
        );
    }
}

#[test]
fn file_search_calls_are_never_limited_mid_turn() {
    let mut tools = ToolAccounting::new(ToolLimits {
        web_search: 2,
        code_interpreter: 10,
    });
    for _ in 0..200 {
        assert_eq!(tools.on_start("file_search"), StartDecision::Relay);
    }
}
