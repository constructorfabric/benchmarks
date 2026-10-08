#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

const ENTRY_YAML: &str = r#"
id: "gpt-4.1"
provider_model_id: "gpt-4.1"
display_name: "GPT-4.1"
description: "Most capable model"
provider_id: "azure_openai"
provider_display_name: "Azure OpenAI"
icon: ""
tier: Premium
enabled: true
system_prompt: "You are a helpful assistant."
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
fn catalog_entry_parses_deployment_yaml() {
    let entry: ModelCatalogEntry = serde_saphyr::from_str(ENTRY_YAML).unwrap();
    assert_eq!(entry.id, "gpt-4.1");
    assert_eq!(entry.tier, ModelTier::Premium);
    assert!(entry.enabled);
    assert!(entry.supports_vision());
    assert!(entry.is_default());
    assert_eq!(entry.max_tool_calls, 10);
    assert!(entry.tool_support().web_search);
    assert_eq!(entry.general_config.api_params.temperature, Some(0.7));
    assert_eq!(entry.estimation_budgets.bytes_per_token_conservative, 4);
}

#[test]
fn catalog_entry_defaults_optional_fields() {
    let entry: ModelCatalogEntry = serde_json::from_value(serde_json::json!({
        "id": "m",
        "provider_model_id": "pm",
        "display_name": "M",
        "provider_id": "openai",
        "provider_display_name": "OpenAI",
        "tier": "standard",
        "context_window": 1000,
        "max_output_tokens": 100,
        "max_input_tokens": 0,
        "input_tokens_credit_multiplier_micro": 1,
        "output_tokens_credit_multiplier_micro": 1,
        "max_num_results": 5,
        "general_config": {"max_file_size_mb": 25}
    }))
    .unwrap();
    assert!(!entry.enabled, "enabled defaults to false");
    assert_eq!(entry.max_tool_calls, 2);
    assert_eq!(entry.web_search_context_size, WebSearchContextSize::Low);
    assert!(entry.preference.is_none());
    assert_eq!(entry.estimation_budgets, EstimationBudgets::default());
}

fn model(id: &str, enabled: bool, is_default: bool) -> ModelCatalogEntry {
    let mut entry: ModelCatalogEntry = serde_saphyr::from_str(ENTRY_YAML).unwrap();
    entry.id = id.to_owned();
    entry.enabled = enabled;
    entry.preference = Some(ModelPreference {
        is_default,
        sort_order: 0,
    });
    entry
}

#[test]
fn default_model_prefers_first_enabled_default_then_first_enabled() {
    let snapshot = PolicySnapshot {
        policy_version: 1,
        model_catalog: vec![
            model("a", false, true),
            model("b", true, false),
            model("c", true, true),
        ],
        kill_switches: KillSwitches::default(),
    };
    assert_eq!(snapshot.default_model().unwrap().id, "c");

    let snapshot = PolicySnapshot {
        policy_version: 1,
        model_catalog: vec![model("a", false, true), model("b", true, false)],
        kill_switches: KillSwitches::default(),
    };
    assert_eq!(snapshot.default_model().unwrap().id, "b");

    let snapshot = PolicySnapshot {
        policy_version: 1,
        model_catalog: vec![model("a", false, true)],
        kill_switches: KillSwitches::default(),
    };
    assert!(snapshot.default_model().is_none());
}

#[test]
fn kill_switches_require_every_field() {
    let err = serde_json::from_value::<KillSwitches>(serde_json::json!({
        "disable_premium_tier": false
    }));
    assert!(err.is_err());
}

#[test]
fn usage_event_dedupe_keys_use_simple_uuids() {
    let t = Uuid::parse_str("f47ac10b-58cc-4372-a567-0e02b2c3d479").unwrap();
    let key = crate::UsageEvent::turn_dedupe_key(t, t, t);
    assert_eq!(
        key,
        "f47ac10b58cc4372a5670e02b2c3d479/f47ac10b58cc4372a5670e02b2c3d479/f47ac10b58cc4372a5670e02b2c3d479"
    );
    let key = crate::UsageEvent::system_task_dedupe_key(t, "thread_summary_update", t);
    assert_eq!(
        key,
        "f47ac10b58cc4372a5670e02b2c3d479/thread_summary_update/f47ac10b58cc4372a5670e02b2c3d479"
    );
}
