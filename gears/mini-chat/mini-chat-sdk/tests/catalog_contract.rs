//! Serde contract of the SDK model-catalog types (`ModelCatalogEntry` and its
//! nested types).
//!
//! Source of truth: DESIGN section "Model Catalog Configuration", Appendix A.1
//! (`GetPolicySnapshot`, required vs. defaulted fields), Appendix B.1 ("nested
//! SDK types do not reject unknown keys") and the catalog entries shipped in
//! `config/mini-chat.yaml`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    clippy::similar_names,
    clippy::use_debug,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value
)]

use mini_chat_sdk::{ModelCatalogEntry, ModelTier, WebSearchContextSize};
use serde_json::{Value, json};

/// The premium entry of `config/mini-chat.yaml`, every key spelled out.
fn full_entry_json() -> Value {
    json!({
        "id": "gpt-4.1",
        "provider_model_id": "gpt-4.1-2025-04-14",
        "display_name": "GPT-4.1",
        "description": "Most capable model",
        "provider_id": "azure_openai",
        "provider_display_name": "Azure OpenAI",
        "icon": "gpt.svg",
        "tier": "Premium",
        "enabled": true,
        "system_prompt": "You are a helpful, concise assistant.",
        "thread_summary_prompt": "Summarize tersely.",
        "multimodal_capabilities": ["VISION_INPUT", "RAG"],
        "context_window": 1_047_576,
        "max_output_tokens": 32768,
        "max_input_tokens": 1_000_000,
        "input_tokens_credit_multiplier_micro": 3_000_000,
        "output_tokens_credit_multiplier_micro": 15_000_000,
        "multiplier_display": "3x",
        "estimation_budgets": {
            "bytes_per_token_conservative": 3,
            "fixed_overhead_tokens": 120,
            "safety_margin_pct": 20,
            "image_token_budget": 900,
            "tool_surcharge_tokens": 450,
            "web_search_surcharge_tokens": 400,
            "code_interpreter_surcharge_tokens": 1100,
            "minimal_generation_floor": 64
        },
        "max_num_results": 5,
        "web_search_context_size": "medium",
        "max_tool_calls": 10,
        "general_config": {
            "type": "model.general.v1",
            "available_from": "1970-01-01T00:00:00Z",
            "max_file_size_mb": 25,
            "api_params": {
                "temperature": 0.7,
                "top_p": 1.0,
                "frequency_penalty": 0.0,
                "presence_penalty": 0.0,
                "stop": []
            },
            "features": { "streaming": true, "structured_output": false },
            "tool_support": {
                "web_search": true,
                "file_search": true,
                "image_generation": false,
                "code_interpreter": true,
                "mcp": false
            },
            "supported_endpoints": {
                "chat_completions": true,
                "responses": true,
                "embeddings": false,
                "image_generation": false,
                "audio_speech_generation": false,
                "audio_transcription": false,
                "audio_translation": false
            }
        },
        "preference": { "is_default": true, "sort_order": 3 }
    })
}

/// Only the fields Appendix A.1 lists as required (no default).
fn minimal_entry_json() -> Value {
    let mut v = full_entry_json();
    let obj = v.as_object_mut().unwrap();
    for key in [
        "description",
        "icon",
        "enabled",
        "system_prompt",
        "thread_summary_prompt",
        "multimodal_capabilities",
        "multiplier_display",
        "estimation_budgets",
        "web_search_context_size",
        "max_tool_calls",
        "preference",
    ] {
        obj.remove(key);
    }
    v
}

fn parse(v: Value) -> Result<ModelCatalogEntry, serde_json::Error> {
    serde_json::from_value(v)
}

#[test]
fn full_entry_exposes_every_field() {
    let e = parse(full_entry_json()).expect("full entry parses");

    assert_eq!(e.id, "gpt-4.1");
    assert_eq!(e.provider_model_id, "gpt-4.1-2025-04-14");
    assert_eq!(e.display_name, "GPT-4.1");
    assert_eq!(e.description, "Most capable model");
    assert_eq!(e.provider_id, "azure_openai");
    assert_eq!(e.provider_display_name, "Azure OpenAI");
    assert_eq!(e.icon, "gpt.svg");
    assert_eq!(e.tier, ModelTier::Premium);
    assert!(e.enabled);
    assert_eq!(e.system_prompt, "You are a helpful, concise assistant.");
    assert_eq!(e.thread_summary_prompt, "Summarize tersely.");
    assert_eq!(e.multimodal_capabilities, vec!["VISION_INPUT", "RAG"]);
    assert_eq!(e.context_window, 1_047_576);
    assert_eq!(e.max_output_tokens, 32768);
    assert_eq!(e.max_input_tokens, 1_000_000);
    assert_eq!(e.input_tokens_credit_multiplier_micro, 3_000_000);
    assert_eq!(e.output_tokens_credit_multiplier_micro, 15_000_000);
    assert_eq!(e.multiplier_display, "3x");
    assert_eq!(e.max_num_results, 5);
    assert_eq!(e.web_search_context_size, WebSearchContextSize::Medium);
    assert_eq!(e.max_tool_calls, 10);

    let b = &e.estimation_budgets;
    assert_eq!(b.bytes_per_token_conservative, 3);
    assert_eq!(b.fixed_overhead_tokens, 120);
    assert_eq!(b.safety_margin_pct, 20);
    assert_eq!(b.image_token_budget, 900);
    assert_eq!(b.tool_surcharge_tokens, 450);
    assert_eq!(b.web_search_surcharge_tokens, 400);
    assert_eq!(b.code_interpreter_surcharge_tokens, 1100);
    assert_eq!(b.minimal_generation_floor, 64);

    let g = &e.general_config;
    assert_eq!(g.max_file_size_mb, 25);
    assert_eq!(g.api_params.temperature, Some(0.7));
    assert_eq!(g.api_params.top_p, Some(1.0));
    assert_eq!(g.api_params.frequency_penalty, Some(0.0));
    assert_eq!(g.api_params.presence_penalty, Some(0.0));
    assert!(g.api_params.stop.is_empty());
    assert!(g.features.streaming);
    assert!(!g.features.structured_output);
    assert!(g.tool_support.web_search);
    assert!(g.tool_support.file_search);
    assert!(!g.tool_support.image_generation);
    assert!(g.tool_support.code_interpreter);
    assert!(!g.tool_support.mcp);
    assert!(g.supported_endpoints.chat_completions);
    assert!(g.supported_endpoints.responses);
    assert!(!g.supported_endpoints.embeddings);
    assert!(!g.supported_endpoints.image_generation);
    assert!(!g.supported_endpoints.audio_speech_generation);
    assert!(!g.supported_endpoints.audio_transcription);
    assert!(!g.supported_endpoints.audio_translation);

    let pref = e.preference.as_ref().expect("preference present");
    assert!(pref.is_default);
    assert_eq!(pref.sort_order, 3);
}

#[test]
fn general_config_type_and_available_from_survive_serialization() {
    let e = parse(full_entry_json()).unwrap();
    let out = serde_json::to_value(&e).unwrap();
    assert_eq!(out["general_config"]["type"], json!("model.general.v1"));
    assert_eq!(
        out["general_config"]["available_from"],
        json!("1970-01-01T00:00:00Z")
    );
}

#[test]
fn entry_with_empty_general_config_type_is_accepted() {
    // config/mini-chat.yaml ships `type: ""`.
    let mut v = full_entry_json();
    v["general_config"]["type"] = json!("");
    let e = parse(v).expect("empty type string is valid");
    let out = serde_json::to_value(&e).unwrap();
    assert_eq!(out["general_config"]["type"], json!(""));
}

#[test]
fn only_required_fields_get_documented_defaults() {
    let e = parse(minimal_entry_json()).expect("minimal entry parses");

    assert_eq!(e.description, "");
    assert_eq!(e.icon, "");
    assert!(!e.enabled, "`enabled` defaults to false");
    assert_eq!(e.system_prompt, "");
    assert_eq!(e.thread_summary_prompt, "");
    assert!(e.multimodal_capabilities.is_empty());
    assert_eq!(e.multiplier_display, "");
    assert_eq!(
        e.web_search_context_size,
        WebSearchContextSize::Low,
        "`web_search_context_size` defaults to low"
    );
    assert_eq!(e.max_tool_calls, 2, "`max_tool_calls` defaults to 2");
    assert!(e.preference.is_none(), "`preference` defaults to none");

    // DESIGN B.5.2 default values, used when the entry omits the object.
    let b = &e.estimation_budgets;
    assert_eq!(b.bytes_per_token_conservative, 4);
    assert_eq!(b.fixed_overhead_tokens, 100);
    assert_eq!(b.safety_margin_pct, 10);
    assert_eq!(b.image_token_budget, 1000);
    assert_eq!(b.tool_surcharge_tokens, 500);
    assert_eq!(b.web_search_surcharge_tokens, 500);
    assert_eq!(b.code_interpreter_surcharge_tokens, 1000);
    assert_eq!(b.minimal_generation_floor, 50);
}

#[test]
fn every_required_field_is_enforced() {
    for key in [
        "id",
        "provider_model_id",
        "display_name",
        "provider_id",
        "provider_display_name",
        "tier",
        "context_window",
        "max_output_tokens",
        "max_input_tokens",
        "input_tokens_credit_multiplier_micro",
        "output_tokens_credit_multiplier_micro",
        "max_num_results",
        "general_config",
    ] {
        let mut v = full_entry_json();
        v.as_object_mut().unwrap().remove(key);
        assert!(
            parse(v).is_err(),
            "entry without required field `{key}` must not deserialize"
        );
    }
}

#[test]
fn required_general_config_members_are_enforced() {
    for key in [
        "type",
        "available_from",
        "max_file_size_mb",
        "api_params",
        "features",
        "tool_support",
        "supported_endpoints",
    ] {
        let mut v = full_entry_json();
        v["general_config"].as_object_mut().unwrap().remove(key);
        assert!(
            parse(v).is_err(),
            "general_config without `{key}` must not deserialize"
        );
    }

    // Nested flags are required as well: a missing switch must not read as false.
    for (section, key) in [
        ("tool_support", "web_search"),
        ("tool_support", "code_interpreter"),
        ("features", "streaming"),
        ("supported_endpoints", "responses"),
    ] {
        let mut v = full_entry_json();
        v["general_config"][section]
            .as_object_mut()
            .unwrap()
            .remove(key);
        assert!(
            parse(v).is_err(),
            "general_config.{section} without `{key}` must not deserialize"
        );
    }
}

#[test]
fn api_params_extra_body_and_reasoning_effort_are_optional() {
    let e = parse(full_entry_json()).expect("parses without extra_body / reasoning_effort");
    assert!(e.general_config.api_params.extra_body.is_none());
    assert!(e.general_config.api_params.reasoning_effort.is_none());

    let mut v = full_entry_json();
    v["general_config"]["api_params"]["extra_body"] = json!({"service_tier": "flex"});
    v["general_config"]["api_params"]["reasoning_effort"] = json!("low");
    let e = parse(v).expect("parses with extra_body / reasoning_effort");
    assert_eq!(
        e.general_config.api_params.extra_body,
        Some(json!({"service_tier": "flex"}))
    );
    assert_eq!(
        e.general_config.api_params.reasoning_effort.as_deref(),
        Some("low")
    );
}

#[test]
fn unknown_keys_inside_nested_sdk_types_are_ignored() {
    let mut v = full_entry_json();
    v["future_entry_field"] = json!(true);
    v["estimation_budgets"]["future_budget"] = json!(7);
    v["general_config"]["future_general_field"] = json!("x");
    let e = parse(v).expect("unknown keys must be ignored, not rejected");
    assert_eq!(e.id, "gpt-4.1");
    assert_eq!(e.estimation_budgets.minimal_generation_floor, 64);
}

#[test]
fn tier_accepts_capitalized_and_lowercase_spellings() {
    for (raw, want) in [
        ("Premium", ModelTier::Premium),
        ("premium", ModelTier::Premium),
        ("Standard", ModelTier::Standard),
        ("standard", ModelTier::Standard),
    ] {
        let mut v = full_entry_json();
        v["tier"] = json!(raw);
        assert_eq!(parse(v).unwrap().tier, want, "tier spelling {raw}");
    }

    let mut v = full_entry_json();
    v["tier"] = json!("platinum");
    assert!(parse(v).is_err(), "unknown tier must be rejected");
}

#[test]
fn web_search_context_size_values() {
    for (raw, want) in [
        ("low", WebSearchContextSize::Low),
        ("medium", WebSearchContextSize::Medium),
        ("high", WebSearchContextSize::High),
    ] {
        let mut v = full_entry_json();
        v["web_search_context_size"] = json!(raw);
        assert_eq!(parse(v).unwrap().web_search_context_size, want);
    }
    let mut v = full_entry_json();
    v["web_search_context_size"] = json!("huge");
    assert!(parse(v).is_err());
}

#[test]
fn credit_multipliers_hold_the_documented_maximum() {
    // DESIGN B.1: multipliers are validated in 1..=10_000_000_000, which does
    // not fit in a u32.
    let mut v = full_entry_json();
    v["input_tokens_credit_multiplier_micro"] = json!(10_000_000_000_u64);
    v["output_tokens_credit_multiplier_micro"] = json!(10_000_000_000_u64);
    let e = parse(v).expect("10_000_000_000 must deserialize");
    assert_eq!(e.input_tokens_credit_multiplier_micro, 10_000_000_000);
    assert_eq!(e.output_tokens_credit_multiplier_micro, 10_000_000_000);
}

#[test]
fn preference_object_is_optional_and_typed() {
    let mut v = full_entry_json();
    v["preference"] = json!({"is_default": false, "sort_order": 0});
    let pref = parse(v).unwrap().preference.unwrap();
    assert!(!pref.is_default);
    assert_eq!(pref.sort_order, 0);

    let mut v = full_entry_json();
    v["preference"] = Value::Null;
    assert!(parse(v).unwrap().preference.is_none());
}

#[test]
fn serialization_round_trips() {
    let e = parse(full_entry_json()).unwrap();
    let again: ModelCatalogEntry =
        serde_json::from_value(serde_json::to_value(&e).unwrap()).expect("round trip");
    assert_eq!(e, again);

    let m = parse(minimal_entry_json()).unwrap();
    let again: ModelCatalogEntry =
        serde_json::from_value(serde_json::to_value(&m).unwrap()).expect("round trip");
    assert_eq!(m, again);
}

#[test]
fn wrong_types_are_rejected() {
    let mut v = full_entry_json();
    v["context_window"] = json!("big");
    assert!(parse(v).is_err());

    let mut v = full_entry_json();
    v["enabled"] = json!("yes");
    assert!(parse(v).is_err());

    let mut v = full_entry_json();
    v["multimodal_capabilities"] = json!("VISION_INPUT");
    assert!(parse(v).is_err());
}
