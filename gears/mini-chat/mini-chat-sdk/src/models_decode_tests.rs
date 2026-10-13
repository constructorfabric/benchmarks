#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Tests of `cpt-cf-mini-chat-algo-gear-foundation-decode-policy-snapshot`.

use serde_json::{Value, json};

use crate::models::{
    ModelTier, WebSearchContextSize, decode_model_catalog_entry, decode_policy_snapshot,
};

const REQUIRED_ENTRY_FIELDS: [&str; 13] = [
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
];

const KILL_SWITCHES: [&str; 6] = [
    "disable_premium_tier",
    "force_standard_tier",
    "disable_web_search",
    "disable_file_search",
    "disable_images",
    "disable_code_interpreter",
];

fn general_config() -> Value {
    json!({
        "type": "model.general.v1",
        "available_from": "2026-01-01T00:00:00Z",
        "max_file_size_mb": 25,
        "api_params": { "stop": [] },
        "features": { "streaming": true, "structured_output": false },
        "tool_support": {
            "web_search": true, "file_search": true, "image_generation": false,
            "code_interpreter": true, "mcp": false
        },
        "supported_endpoints": {
            "chat_completions": false, "responses": true, "embeddings": false,
            "image_generation": false, "audio_speech_generation": false,
            "audio_transcription": false, "audio_translation": false
        }
    })
}

/// Entry carrying only the required fields.
fn minimal_entry() -> Value {
    json!({
        "id": "gpt-5.2",
        "provider_model_id": "gpt-5.2",
        "display_name": "GPT-5.2",
        "provider_id": "azure_openai",
        "provider_display_name": "Azure OpenAI",
        "tier": "premium",
        "context_window": 128_000,
        "max_output_tokens": 4096,
        "max_input_tokens": 120_000,
        "input_tokens_credit_multiplier_micro": 2_500_000,
        "output_tokens_credit_multiplier_micro": 2_500_000,
        "max_num_results": 5,
        "general_config": general_config(),
    })
}

fn kill_switches() -> Value {
    json!({
        "disable_premium_tier": false,
        "force_standard_tier": false,
        "disable_web_search": false,
        "disable_file_search": false,
        "disable_images": false,
        "disable_code_interpreter": false
    })
}

fn snapshot_bytes(entries: &[Value], switches: &Value) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "policy_version": 12345,
        "model_catalog": entries,
        "kill_switches": switches,
    }))
    .unwrap()
}

#[test]
fn entry_with_only_required_fields_gets_documented_defaults() {
    let entry = decode_model_catalog_entry(&minimal_entry()).expect("decodes");

    assert_eq!(entry.description, "");
    assert_eq!(entry.icon, "");
    assert_eq!(entry.multiplier_display, "");
    assert_eq!(entry.system_prompt, "");
    assert_eq!(entry.thread_summary_prompt, "");
    assert!(!entry.enabled);
    assert!(entry.multimodal_capabilities.is_empty());
    assert_eq!(entry.web_search_context_size, WebSearchContextSize::Low);
    assert_eq!(entry.max_tool_calls, 2);
    assert_eq!(entry.preference, None);
}

#[test]
fn entry_without_estimation_budgets_uses_default_budgets() {
    let entry = decode_model_catalog_entry(&minimal_entry()).expect("decodes");
    let b = entry.estimation_budgets;

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
fn partial_estimation_budgets_fill_each_absent_field_individually() {
    let mut value = minimal_entry();
    value["estimation_budgets"] =
        json!({ "bytes_per_token_conservative": 3, "image_token_budget": 2000 });

    let b = decode_model_catalog_entry(&value)
        .expect("decodes")
        .estimation_budgets;

    assert_eq!(b.bytes_per_token_conservative, 3);
    assert_eq!(b.image_token_budget, 2000);
    assert_eq!(b.fixed_overhead_tokens, 100);
    assert_eq!(b.safety_margin_pct, 10);
    assert_eq!(b.tool_surcharge_tokens, 500);
    assert_eq!(b.web_search_surcharge_tokens, 500);
    assert_eq!(b.code_interpreter_surcharge_tokens, 1000);
    assert_eq!(b.minimal_generation_floor, 50);
}

#[test]
fn each_required_entry_field_missing_is_a_decode_error_naming_the_field() {
    for field in REQUIRED_ENTRY_FIELDS {
        let mut value = minimal_entry();
        value.as_object_mut().unwrap().remove(field);

        let err =
            decode_model_catalog_entry(&value).expect_err(&format!("missing `{field}` must fail"));

        assert!(
            err.message.contains(&format!("`{field}`")),
            "error for missing `{field}` must name it, got: {}",
            err.message
        );
    }
}

#[test]
fn general_config_fields_are_required_except_optional_api_params() {
    for field in [
        "type",
        "available_from",
        "max_file_size_mb",
        "api_params",
        "features",
        "tool_support",
        "supported_endpoints",
    ] {
        let mut value = minimal_entry();
        value["general_config"]
            .as_object_mut()
            .unwrap()
            .remove(field);
        let err = decode_model_catalog_entry(&value)
            .expect_err(&format!("general_config.{field} is required"));
        assert!(
            err.message.contains(&format!("`{field}`")),
            "got: {}",
            err.message
        );
    }

    let mut value = minimal_entry();
    value["general_config"]["api_params"]
        .as_object_mut()
        .unwrap()
        .remove("stop");
    assert!(
        decode_model_catalog_entry(&value).is_err(),
        "api_params.stop is required"
    );
}

#[test]
fn api_params_sampling_extra_body_and_reasoning_effort_are_optional() {
    // `general_config()` already omits all of them.
    let entry = decode_model_catalog_entry(&minimal_entry()).expect("decodes");
    let p = entry.general_config.api_params;

    assert_eq!(p.temperature, None);
    assert_eq!(p.top_p, None);
    assert_eq!(p.frequency_penalty, None);
    assert_eq!(p.presence_penalty, None);
    assert_eq!(p.extra_body, None);
    assert_eq!(p.reasoning_effort, None);
    assert!(p.stop.is_empty());
}

#[test]
fn tier_accepts_lowercase_and_capitalized_forms() {
    for (raw, want) in [
        ("premium", ModelTier::Premium),
        ("standard", ModelTier::Standard),
        ("Premium", ModelTier::Premium),
        ("Standard", ModelTier::Standard),
    ] {
        let mut value = minimal_entry();
        value["tier"] = json!(raw);
        let entry = decode_model_catalog_entry(&value).expect("tier decodes");
        assert_eq!(entry.tier, want, "tier {raw}");
    }
}

#[test]
fn unknown_tier_is_a_decode_error() {
    for raw in ["gold", "PREMIUM", ""] {
        let mut value = minimal_entry();
        value["tier"] = json!(raw);
        assert!(
            decode_model_catalog_entry(&value).is_err(),
            "tier {raw:?} must fail"
        );
    }
}

#[test]
fn unknown_keys_inside_entry_and_nested_objects_are_ignored() {
    let mut value = minimal_entry();
    value["future_field"] = json!({ "a": 1 });
    value["estimation_budgets"] = json!({ "future_budget": 7 });
    value["general_config"]["future_gc"] = json!(true);
    value["general_config"]["tool_support"]["future_tool"] = json!(true);
    value["general_config"]["api_params"]["future_param"] = json!(1);
    value["preference"] = json!({ "is_default": true, "sort_order": 3, "future": 1 });

    let entry = decode_model_catalog_entry(&value).expect("unknown keys are ignored");

    assert_eq!(entry.id, "gpt-5.2");
    assert_eq!(entry.preference.map(|p| p.sort_order), Some(3));
}

#[test]
fn snapshot_with_all_six_switches_decodes() {
    let bytes = snapshot_bytes(&[minimal_entry()], &kill_switches());

    let snapshot = decode_policy_snapshot(&bytes).expect("decodes");

    assert_eq!(snapshot.policy_version, 12345);
    assert_eq!(snapshot.model_catalog.len(), 1);
    assert!(!snapshot.kill_switches.disable_premium_tier);
    assert!(!snapshot.kill_switches.disable_code_interpreter);
}

#[test]
fn snapshot_kill_switch_values_are_read_not_defaulted() {
    let mut switches = kill_switches();
    switches["force_standard_tier"] = json!(true);
    switches["disable_images"] = json!(true);

    let snapshot = decode_policy_snapshot(&snapshot_bytes(&[], &switches)).expect("decodes");

    assert!(snapshot.kill_switches.force_standard_tier);
    assert!(snapshot.kill_switches.disable_images);
    assert!(!snapshot.kill_switches.disable_web_search);
    assert!(snapshot.model_catalog.is_empty());
}

#[test]
fn each_missing_kill_switch_fails_and_is_never_read_as_false() {
    for switch in KILL_SWITCHES {
        let mut switches = kill_switches();
        switches.as_object_mut().unwrap().remove(switch);

        let err = decode_policy_snapshot(&snapshot_bytes(&[], &switches))
            .expect_err(&format!("missing `{switch}` must fail"));

        assert!(
            err.message.contains(&format!("`{switch}`")),
            "error for missing `{switch}` must name it, got: {}",
            err.message
        );
    }
}

#[test]
fn absent_kill_switches_object_fails() {
    let bytes = serde_json::to_vec(&json!({ "policy_version": 1, "model_catalog": [] })).unwrap();

    let err = decode_policy_snapshot(&bytes).expect_err("kill_switches is required");

    assert!(
        err.message.contains("`kill_switches`"),
        "got: {}",
        err.message
    );
}

#[test]
fn snapshot_with_bad_entry_fails_naming_the_field() {
    let mut entry = minimal_entry();
    entry.as_object_mut().unwrap().remove("provider_model_id");

    let err = decode_policy_snapshot(&snapshot_bytes(&[entry], &kill_switches()))
        .expect_err("entry without provider_model_id must fail");

    assert!(
        err.message.contains("`provider_model_id`"),
        "got: {}",
        err.message
    );
}

#[test]
fn malformed_snapshot_payload_fails() {
    assert!(decode_policy_snapshot(b"not json").is_err());
    assert!(decode_policy_snapshot(b"{}").is_err());
}

/// Entry copied from `config/mini-chat.yaml` (`static-mini-chat-model-policy-plugin`):
/// capitalized tier, all budgets, `type: ""`, `stop: []`, preference.
const YAML_ENTRY: &str = r#"
id: "gpt-4.1"
provider_model_id: "gpt-4.1"
display_name: "GPT-4.1"
description: "Most capable model"
provider_id: "azure_openai"
provider_display_name: "Azure OpenAI"
icon: ""
tier: Premium
enabled: true
system_prompt: "You are a helpful, concise assistant. Answer clearly and directly."
thread_summary_prompt: ""
multimodal_capabilities: ["VISION_INPUT"]
context_window: 1047576
max_output_tokens: 32768
max_input_tokens: 1047576
input_tokens_credit_multiplier_micro: 3000000
output_tokens_credit_multiplier_micro: 15000000
multiplier_display: "3x"
estimation_budgets:
  bytes_per_token_conservative: 4
  fixed_overhead_tokens: 100
  safety_margin_pct: 10
  image_token_budget: 1000
  tool_surcharge_tokens: 500
  web_search_surcharge_tokens: 500
  code_interpreter_surcharge_tokens: 1000
  minimal_generation_floor: 50
max_num_results: 5
web_search_context_size: low
max_tool_calls: 10
general_config:
  type: ""
  available_from: "1970-01-01T00:00:00Z"
  max_file_size_mb: 25
  api_params:
    temperature: 0.7
    top_p: 1.0
    frequency_penalty: 0.0
    presence_penalty: 0.0
    stop: []
  features:
    streaming: true
    structured_output: true
  tool_support:
    web_search: true
    file_search: true
    image_generation: false
    code_interpreter: true
    mcp: false
  supported_endpoints:
    chat_completions: true
    responses: true
    embeddings: false
    image_generation: false
    audio_speech_generation: false
    audio_transcription: false
    audio_translation: false
preference:
  is_default: true
  sort_order: 0
"#;

#[test]
fn yaml_fixture_parses_into_a_json_value() {
    let value: Value = serde_saphyr::from_str(YAML_ENTRY).expect("fixture is valid YAML");
    assert_eq!(value["tier"], json!("Premium"));
}

#[test]
fn config_yaml_style_entry_decodes() {
    let value: Value = serde_saphyr::from_str(YAML_ENTRY).expect("fixture is valid YAML");

    let entry = decode_model_catalog_entry(&value).expect("config/mini-chat.yaml entry decodes");

    assert_eq!(entry.id, "gpt-4.1");
    assert_eq!(entry.tier, ModelTier::Premium);
    assert!(entry.enabled);
    assert_eq!(entry.max_tool_calls, 10);
    assert_eq!(
        entry.multimodal_capabilities,
        vec!["VISION_INPUT".to_owned()]
    );
    assert_eq!(entry.general_config.config_type, "");
    assert_eq!(entry.general_config.max_file_size_mb, 25);
    assert_eq!(entry.general_config.api_params.temperature, Some(0.7));
    assert!(entry.general_config.tool_support.code_interpreter);
    assert!(!entry.general_config.tool_support.mcp);
    assert_eq!(entry.preference.map(|p| p.is_default), Some(true));
    assert_eq!(entry.input_tokens_credit_multiplier_micro, 3_000_000);
    assert_eq!(entry.output_tokens_credit_multiplier_micro, 15_000_000);
}
