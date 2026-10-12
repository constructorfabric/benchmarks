#![allow(clippy::unwrap_used, clippy::expect_used)]

//! S1 SDK model tests: `ModelCatalogEntry` deserialization (DESIGN "Model Catalog
//! Configuration", section 5.2.1, Appendix B.1 bundled plugins), `KillSwitches`
//! strictness, the `UsageEvent` JSON payload (sections 5.6-5.7 and "System Task
//! Attribution Rules") and the outbox dedupe key helpers.

use mini_chat_sdk::{
    EstimationBudgets, KillSwitches, ModelCatalogEntry, ModelTier, UsageEvent, UsageTokens,
    dedupe_key_for_system_task, dedupe_key_for_turn,
};
use serde_json::{Map, Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

const TENANT: &str = "f47ac10b-58cc-4372-a567-0e02b2c3d479";
const TURN: &str = "0b6e0f6a-1c2d-4e3f-8a9b-0c1d2e3f4a5b";
const REQUEST: &str = "9a8b7c6d-5e4f-4a3b-9c2d-1e0f9a8b7c6d";
const USER: &str = "11111111-2222-4333-8444-555555555555";
const CHAT: &str = "66666666-7777-4888-9999-aaaaaaaaaaaa";

const KILL_SWITCH_FIELDS: [&str; 6] = [
    "disable_premium_tier",
    "force_standard_tier",
    "disable_web_search",
    "disable_file_search",
    "disable_images",
    "disable_code_interpreter",
];

fn parse_uuid(text: &str) -> Uuid {
    Uuid::parse_str(text).expect("test UUID literal must parse")
}

fn approx_eq(left: f64, right: f64) -> bool {
    (left - right).abs() < 1e-9
}

/// The `gpt-4.1` entry of the `static-mini-chat-model-policy-plugin` catalog in
/// `config/mini-chat.yaml`, as JSON.
fn yaml_gpt41_entry() -> Value {
    json!({
        "id": "gpt-4.1",
        "provider_model_id": "gpt-4.1",
        "display_name": "GPT-4.1",
        "description": "Most capable model",
        "provider_id": "azure_openai",
        "provider_display_name": "Azure OpenAI",
        "icon": "",
        "tier": "Premium",
        "enabled": true,
        "system_prompt": "You are a helpful, concise assistant. Answer clearly and directly.",
        "thread_summary_prompt": "",
        "multimodal_capabilities": ["VISION_INPUT"],
        "context_window": 1_047_576,
        "max_output_tokens": 32_768,
        "max_input_tokens": 1_047_576,
        "input_tokens_credit_multiplier_micro": 3_000_000,
        "output_tokens_credit_multiplier_micro": 15_000_000,
        "multiplier_display": "3x",
        "estimation_budgets": {
            "bytes_per_token_conservative": 4,
            "fixed_overhead_tokens": 100,
            "safety_margin_pct": 10,
            "image_token_budget": 1000,
            "tool_surcharge_tokens": 500,
            "web_search_surcharge_tokens": 500,
            "code_interpreter_surcharge_tokens": 1000,
            "minimal_generation_floor": 50
        },
        "max_num_results": 5,
        "web_search_context_size": "low",
        "max_tool_calls": 10,
        "general_config": {
            "type": "",
            "available_from": "1970-01-01T00:00:00Z",
            "max_file_size_mb": 25,
            "api_params": {
                "temperature": 0.7,
                "top_p": 1.0,
                "frequency_penalty": 0.0,
                "presence_penalty": 0.0,
                "stop": []
            },
            "features": {
                "streaming": true,
                "structured_output": true
            },
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
        "preference": {
            "is_default": true,
            "sort_order": 0
        }
    })
}

/// The `gpt-4.1-mini` entry of the YAML catalog, reduced to the fields this
/// suite asserts on (the remaining keys are covered by `yaml_gpt41_entry`).
fn yaml_gpt41_mini_entry() -> Value {
    json!({
        "id": "gpt-4.1-mini",
        "provider_model_id": "gpt-4.1-mini",
        "display_name": "GPT-4.1 Mini",
        "description": "Fast and efficient model",
        "provider_id": "azure_openai",
        "provider_display_name": "Azure OpenAI",
        "icon": "",
        "tier": "Standard",
        "enabled": true,
        "multimodal_capabilities": ["VISION_INPUT"],
        "context_window": 1_047_576,
        "max_output_tokens": 32_768,
        "max_input_tokens": 1_047_576,
        "input_tokens_credit_multiplier_micro": 1_000_000,
        "output_tokens_credit_multiplier_micro": 3_000_000,
        "multiplier_display": "1x",
        "max_tool_calls": 10,
        "preference": {
            "is_default": false,
            "sort_order": 1
        }
    })
}

/// Only the keys without a serde default.
fn minimal_entry() -> Value {
    json!({
        "id": "m-1",
        "provider_model_id": "provider-m-1",
        "display_name": "Model One",
        "provider_id": "openai",
        "tier": "Standard",
        "context_window": 128_000,
        "max_output_tokens": 16_384,
        "input_tokens_credit_multiplier_micro": 1_000_000,
        "output_tokens_credit_multiplier_micro": 2_000_000
    })
}

fn minimal_entry_with(key: &str, value: Value) -> Value {
    let mut entry = minimal_entry();
    entry
        .as_object_mut()
        .expect("entry is an object")
        .insert(key.to_owned(), value);
    entry
}

fn parse_entry(raw: Value) -> ModelCatalogEntry {
    serde_json::from_value(raw).expect("catalog entry must deserialize")
}

fn all_kill_switches() -> Map<String, Value> {
    KILL_SWITCH_FIELDS
        .iter()
        .map(|field| ((*field).to_owned(), Value::Bool(false)))
        .collect()
}

fn fixed_timestamp() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_790_000_000).expect("valid unix timestamp")
}

fn user_turn_event() -> UsageEvent {
    let tenant_id = parse_uuid(TENANT);
    let turn_id = parse_uuid(TURN);
    let request_id = parse_uuid(REQUEST);
    UsageEvent {
        tenant_id,
        user_id: Some(parse_uuid(USER)),
        chat_id: Some(parse_uuid(CHAT)),
        turn_id: Some(turn_id),
        request_id,
        effective_model: "gpt-4.1-mini".to_owned(),
        selected_model: "gpt-4.1".to_owned(),
        terminal_state: "failed".to_owned(),
        billing_outcome: "failed".to_owned(),
        usage: None,
        actual_credits_micro: 1_500,
        settlement_method: "estimated".to_owned(),
        policy_version_applied: 1,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        file_search_calls: 0,
        timestamp: fixed_timestamp(),
        requester_type: "user".to_owned(),
        dedupe_key: dedupe_key_for_turn(tenant_id, turn_id, request_id),
        system_task_type: None,
    }
}

fn system_task_event() -> UsageEvent {
    let tenant_id = parse_uuid(TENANT);
    let system_request_id = parse_uuid(REQUEST);
    UsageEvent {
        tenant_id,
        user_id: None,
        chat_id: Some(parse_uuid(CHAT)),
        turn_id: None,
        request_id: system_request_id,
        effective_model: "gpt-4.1-mini".to_owned(),
        selected_model: "gpt-4.1-mini".to_owned(),
        terminal_state: "completed".to_owned(),
        billing_outcome: "system_task".to_owned(),
        usage: Some(UsageTokens {
            input_tokens: 5_000,
            output_tokens: 200,
            cache_read_input_tokens: 4_000,
            cache_write_input_tokens: 0,
            reasoning_tokens: 0,
        }),
        actual_credits_micro: 0,
        settlement_method: "none".to_owned(),
        policy_version_applied: 0,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        file_search_calls: 0,
        timestamp: fixed_timestamp(),
        requester_type: "system".to_owned(),
        dedupe_key: dedupe_key_for_system_task(
            tenant_id,
            "thread_summary_update",
            system_request_id,
        ),
        system_task_type: Some("thread_summary_update".to_owned()),
    }
}

fn to_object(event: &UsageEvent) -> Map<String, Value> {
    match serde_json::to_value(event).expect("UsageEvent must serialize") {
        Value::Object(map) => map,
        other => panic!("UsageEvent must serialize to a JSON object, got {other}"),
    }
}

// ── ModelCatalogEntry ────────────────────────────────────────────────────

#[test]
fn catalog_entry_yaml_premium_entry_deserializes_identity_and_tier() {
    let entry = parse_entry(yaml_gpt41_entry());

    assert_eq!(entry.id, "gpt-4.1");
    assert_eq!(entry.provider_model_id, "gpt-4.1");
    assert_eq!(entry.display_name, "GPT-4.1");
    assert_eq!(entry.description.as_deref(), Some("Most capable model"));
    assert_eq!(entry.provider_id, "azure_openai");
    assert_eq!(entry.provider_display_name, "Azure OpenAI");
    assert_eq!(entry.tier, ModelTier::Premium);
    assert!(entry.enabled);
    assert_eq!(entry.multimodal_capabilities, ["VISION_INPUT"]);
}

#[test]
fn catalog_entry_yaml_premium_entry_deserializes_limits_and_multipliers() {
    let entry = parse_entry(yaml_gpt41_entry());

    assert_eq!(entry.context_window, 1_047_576);
    assert_eq!(entry.max_output_tokens, 32_768);
    assert_eq!(entry.max_input_tokens, 1_047_576);
    assert_eq!(entry.input_tokens_credit_multiplier_micro, 3_000_000);
    assert_eq!(entry.output_tokens_credit_multiplier_micro, 15_000_000);
    assert_eq!(entry.multiplier_display, "3x");
    assert_eq!(entry.max_num_results, 5);
    assert_eq!(entry.web_search_context_size, "low");
    assert_eq!(entry.max_tool_calls, 10);
}

#[test]
fn catalog_entry_yaml_premium_entry_deserializes_general_config() {
    let entry = parse_entry(yaml_gpt41_entry());
    let general = &entry.general_config;

    assert!(general.r#type.is_empty());
    assert_eq!(
        general.available_from.as_deref(),
        Some("1970-01-01T00:00:00Z")
    );
    assert_eq!(general.max_file_size_mb, 25);
    let temperature = general.api_params.temperature.expect("temperature is set");
    assert!(approx_eq(temperature, 0.7), "temperature: {temperature}");
    let top_p = general.api_params.top_p.expect("top_p is set");
    assert!(approx_eq(top_p, 1.0), "top_p: {top_p}");
    assert!(general.api_params.stop.is_empty());
    assert!(general.api_params.extra_body.is_none());
    assert!(general.api_params.reasoning_effort.is_none());
    assert!(general.tool_support.web_search);
    assert!(general.tool_support.file_search);
    assert!(!general.tool_support.image_generation);
    assert!(general.tool_support.code_interpreter);
    assert!(!general.tool_support.mcp);
}

#[test]
fn catalog_entry_yaml_premium_entry_deserializes_preference() {
    let entry = parse_entry(yaml_gpt41_entry());
    let preference = entry.preference.as_ref().expect("preference is set");

    assert!(preference.is_default);
    assert_eq!(preference.sort_order, 0);
}

#[test]
fn catalog_entry_yaml_premium_entry_deserializes_estimation_budgets() {
    let entry = parse_entry(yaml_gpt41_entry());
    let budgets = &entry.estimation_budgets;

    assert_eq!(budgets.bytes_per_token_conservative, 4);
    assert_eq!(budgets.fixed_overhead_tokens, 100);
    assert_eq!(budgets.safety_margin_pct, 10);
    assert_eq!(budgets.image_token_budget, 1000);
    assert_eq!(budgets.tool_surcharge_tokens, 500);
    assert_eq!(budgets.web_search_surcharge_tokens, 500);
    assert_eq!(budgets.code_interpreter_surcharge_tokens, 1000);
    assert_eq!(budgets.minimal_generation_floor, 50);
}

#[test]
fn catalog_entry_yaml_standard_entry_deserializes_tier_and_multipliers() {
    let entry = parse_entry(yaml_gpt41_mini_entry());

    assert_eq!(entry.id, "gpt-4.1-mini");
    assert_eq!(entry.tier, ModelTier::Standard);
    assert_eq!(entry.input_tokens_credit_multiplier_micro, 1_000_000);
    assert_eq!(entry.output_tokens_credit_multiplier_micro, 3_000_000);
    let preference = entry.preference.as_ref().expect("preference is set");
    assert!(!preference.is_default);
    assert_eq!(preference.sort_order, 1);
}

#[test]
fn catalog_entry_enabled_defaults_to_false_when_omitted() {
    let entry = parse_entry(minimal_entry());

    assert!(!entry.enabled);
}

#[test]
fn catalog_entry_max_tool_calls_defaults_to_two_when_omitted() {
    let entry = parse_entry(minimal_entry());

    assert_eq!(entry.max_tool_calls, 2);
}

#[test]
fn catalog_entry_estimation_budgets_default_when_omitted() {
    let entry = parse_entry(minimal_entry());
    let budgets = &entry.estimation_budgets;

    assert_eq!(budgets.bytes_per_token_conservative, 4);
    assert_eq!(budgets.fixed_overhead_tokens, 100);
    assert_eq!(budgets.safety_margin_pct, 10);
    assert_eq!(budgets.image_token_budget, 1000);
    assert_eq!(budgets.tool_surcharge_tokens, 500);
    assert_eq!(budgets.web_search_surcharge_tokens, 500);
    assert_eq!(budgets.code_interpreter_surcharge_tokens, 1000);
    assert_eq!(budgets.minimal_generation_floor, 50);
}

#[test]
fn catalog_entry_partial_estimation_budgets_fill_missing_fields_with_defaults() {
    let entry = parse_entry(minimal_entry_with(
        "estimation_budgets",
        json!({ "bytes_per_token_conservative": 3, "safety_margin_pct": 20 }),
    ));
    let budgets = &entry.estimation_budgets;

    assert_eq!(budgets.bytes_per_token_conservative, 3);
    assert_eq!(budgets.safety_margin_pct, 20);
    assert_eq!(budgets.fixed_overhead_tokens, 100);
    assert_eq!(budgets.image_token_budget, 1000);
    assert_eq!(budgets.minimal_generation_floor, 50);
}

#[test]
fn catalog_entry_optional_fields_default_when_omitted() {
    let entry = parse_entry(minimal_entry());

    assert!(entry.description.is_none());
    assert!(entry.provider_display_name.is_empty());
    assert!(entry.icon.is_empty());
    assert!(entry.system_prompt.is_empty());
    assert!(entry.thread_summary_prompt.is_empty());
    assert!(entry.multimodal_capabilities.is_empty());
    assert_eq!(entry.max_input_tokens, 0);
    assert!(entry.multiplier_display.is_empty());
    assert_eq!(entry.max_num_results, 5);
    assert_eq!(entry.web_search_context_size, "low");
    assert!(entry.preference.is_none());
}

#[test]
fn catalog_entry_tool_support_defaults_to_all_false_when_omitted() {
    let entry = parse_entry(minimal_entry());
    let tools = &entry.general_config.tool_support;

    assert!(!tools.web_search);
    assert!(!tools.file_search);
    assert!(!tools.image_generation);
    assert!(!tools.code_interpreter);
    assert!(!tools.mcp);
}

#[test]
fn catalog_entry_general_config_max_file_size_defaults_to_25_mb() {
    let entry = parse_entry(minimal_entry_with("general_config", json!({ "type": "" })));

    assert_eq!(entry.general_config.max_file_size_mb, 25);
    assert!(entry.general_config.api_params.temperature.is_none());
    assert!(entry.general_config.api_params.stop.is_empty());
}

#[test]
fn catalog_entry_ignores_unknown_keys_at_every_level() {
    let mut raw = minimal_entry();
    let object = raw.as_object_mut().expect("entry is an object");
    object.insert("future_top_level_key".to_owned(), json!("x"));
    object.insert(
        "estimation_budgets".to_owned(),
        json!({ "bytes_per_token_conservative": 4, "future_budget": 7 }),
    );
    object.insert(
        "general_config".to_owned(),
        json!({
            "type": "",
            "future_general": true,
            "api_params": { "future_param": 1 },
            "tool_support": { "web_search": true, "future_tool": true }
        }),
    );

    let result = serde_json::from_value::<ModelCatalogEntry>(raw);

    let entry = result.expect("unknown keys must be ignored");
    assert!(entry.general_config.tool_support.web_search);
}

#[test]
fn catalog_entry_missing_required_key_is_rejected() {
    for required in [
        "id",
        "provider_model_id",
        "display_name",
        "provider_id",
        "tier",
        "context_window",
        "max_output_tokens",
        "input_tokens_credit_multiplier_micro",
        "output_tokens_credit_multiplier_micro",
    ] {
        let mut raw = minimal_entry();
        raw.as_object_mut()
            .expect("entry is an object")
            .remove(required);

        let result = serde_json::from_value::<ModelCatalogEntry>(raw);

        assert!(
            result.is_err(),
            "entry without `{required}` must be rejected"
        );
    }
}

#[test]
fn catalog_entry_accepts_lowercase_tier() {
    let entry = parse_entry(minimal_entry_with("tier", json!("premium")));

    assert_eq!(entry.tier, ModelTier::Premium);
}

#[test]
fn catalog_entry_serde_round_trip_preserves_core_fields() {
    let original = parse_entry(yaml_gpt41_entry());

    let encoded = serde_json::to_value(&original).expect("entry must serialize");
    let decoded = parse_entry(encoded);

    assert_eq!(decoded.id, original.id);
    assert_eq!(decoded.tier, original.tier);
    assert_eq!(decoded.enabled, original.enabled);
    assert_eq!(
        decoded.input_tokens_credit_multiplier_micro,
        original.input_tokens_credit_multiplier_micro
    );
    assert_eq!(
        decoded.output_tokens_credit_multiplier_micro,
        original.output_tokens_credit_multiplier_micro
    );
    assert_eq!(decoded.max_tool_calls, original.max_tool_calls);
}

// ── ModelTier / EstimationBudgets ────────────────────────────────────────

#[test]
fn model_tier_accepts_capitalized_and_lowercase_names() {
    let cases = [
        ("Premium", ModelTier::Premium),
        ("premium", ModelTier::Premium),
        ("Standard", ModelTier::Standard),
        ("standard", ModelTier::Standard),
    ];
    for (raw, expected) in cases {
        let tier: ModelTier =
            serde_json::from_value(json!(raw)).expect("tier name must deserialize");
        assert_eq!(tier, expected, "tier name {raw}");
    }
}

#[test]
fn model_tier_rejects_unknown_name() {
    let result = serde_json::from_value::<ModelTier>(json!("gold"));

    assert!(result.is_err());
}

#[test]
fn estimation_budgets_default_matches_design_values() {
    let budgets = EstimationBudgets::default();

    assert_eq!(budgets.bytes_per_token_conservative, 4);
    assert_eq!(budgets.fixed_overhead_tokens, 100);
    assert_eq!(budgets.safety_margin_pct, 10);
    assert_eq!(budgets.image_token_budget, 1000);
    assert_eq!(budgets.tool_surcharge_tokens, 500);
    assert_eq!(budgets.web_search_surcharge_tokens, 500);
    assert_eq!(budgets.code_interpreter_surcharge_tokens, 1000);
    assert_eq!(budgets.minimal_generation_floor, 50);
}

// ── KillSwitches ─────────────────────────────────────────────────────────

#[test]
fn kill_switches_with_every_field_deserialize() {
    let mut raw = all_kill_switches();
    raw.insert("disable_web_search".to_owned(), Value::Bool(true));

    let switches: KillSwitches =
        serde_json::from_value(Value::Object(raw)).expect("complete kill switches");

    assert!(switches.disable_web_search);
    assert!(!switches.disable_premium_tier);
    assert!(!switches.force_standard_tier);
    assert!(!switches.disable_file_search);
    assert!(!switches.disable_images);
    assert!(!switches.disable_code_interpreter);
}

#[test]
fn kill_switches_missing_any_field_is_rejected() {
    for missing in KILL_SWITCH_FIELDS {
        let mut raw = all_kill_switches();
        raw.remove(missing);

        let result = serde_json::from_value::<KillSwitches>(Value::Object(raw));

        assert!(
            result.is_err(),
            "kill switches without `{missing}` must be rejected"
        );
    }
}

#[test]
fn kill_switches_default_is_all_false() {
    let switches = KillSwitches::default();

    assert!(!switches.disable_premium_tier);
    assert!(!switches.force_standard_tier);
    assert!(!switches.disable_web_search);
    assert!(!switches.disable_file_search);
    assert!(!switches.disable_images);
    assert!(!switches.disable_code_interpreter);
}

// ── UsageEvent JSON payload ──────────────────────────────────────────────

#[test]
fn usage_event_user_turn_includes_user_and_turn_ids() {
    let object = to_object(&user_turn_event());

    assert_eq!(object.get("user_id"), Some(&json!(USER)));
    assert_eq!(object.get("turn_id"), Some(&json!(TURN)));
    assert_eq!(object.get("chat_id"), Some(&json!(CHAT)));
    assert_eq!(object.get("tenant_id"), Some(&json!(TENANT)));
    assert_eq!(object.get("request_id"), Some(&json!(REQUEST)));
}

#[test]
fn usage_event_without_usage_serializes_explicit_null() {
    let object = to_object(&user_turn_event());

    assert_eq!(object.get("usage"), Some(&Value::Null));
}

#[test]
fn usage_event_user_turn_omits_system_task_type() {
    let object = to_object(&user_turn_event());

    assert!(!object.contains_key("system_task_type"));
}

#[test]
fn usage_event_carries_every_design_payload_field() {
    let object = to_object(&system_task_event());

    for field in [
        "tenant_id",
        "chat_id",
        "request_id",
        "effective_model",
        "selected_model",
        "terminal_state",
        "billing_outcome",
        "usage",
        "actual_credits_micro",
        "settlement_method",
        "policy_version_applied",
        "web_search_calls",
        "code_interpreter_calls",
        "file_search_calls",
        "timestamp",
        "requester_type",
        "dedupe_key",
        "system_task_type",
    ] {
        assert!(object.contains_key(field), "payload must carry `{field}`");
    }
    assert!(!object.contains_key("event_type"));
}

#[test]
fn usage_event_system_task_omits_user_and_turn_ids() {
    let object = to_object(&system_task_event());

    assert!(
        !object.contains_key("user_id"),
        "user_id must be absent, not null"
    );
    assert!(
        !object.contains_key("turn_id"),
        "turn_id must be absent, not null"
    );
}

#[test]
fn usage_event_system_task_includes_task_type_and_usage() {
    let object = to_object(&system_task_event());

    assert_eq!(
        object.get("system_task_type"),
        Some(&json!("thread_summary_update"))
    );
    assert_eq!(object.get("billing_outcome"), Some(&json!("system_task")));
    assert_eq!(object.get("requester_type"), Some(&json!("system")));
    assert_eq!(
        object.get("usage"),
        Some(&json!({
            "input_tokens": 5_000,
            "output_tokens": 200,
            "cache_read_input_tokens": 4_000,
            "cache_write_input_tokens": 0,
            "reasoning_tokens": 0
        }))
    );
}

#[test]
fn usage_event_without_chat_omits_chat_id() {
    let mut event = system_task_event();
    event.chat_id = None;

    let object = to_object(&event);

    assert!(!object.contains_key("chat_id"));
}

#[test]
fn usage_event_timestamp_serializes_as_rfc3339() {
    let event = user_turn_event();
    let object = to_object(&event);

    let raw = object
        .get("timestamp")
        .and_then(Value::as_str)
        .expect("timestamp must be a JSON string");
    let parsed = OffsetDateTime::parse(raw, &Rfc3339).expect("timestamp must be RFC 3339");

    assert_eq!(parsed, event.timestamp);
}

#[test]
fn usage_event_design_system_task_payload_deserializes() {
    let system_request_hex = parse_uuid(REQUEST).simple().to_string();
    let tenant_hex = parse_uuid(TENANT).simple().to_string();
    let payload = json!({
        "tenant_id": TENANT,
        "chat_id": CHAT,
        "request_id": REQUEST,
        "effective_model": "gpt-4.1-mini",
        "selected_model": "gpt-4.1-mini",
        "terminal_state": "completed",
        "billing_outcome": "system_task",
        "usage": {
            "input_tokens": 5_000,
            "output_tokens": 200,
            "cache_read_input_tokens": 4_000,
            "cache_write_input_tokens": 0,
            "reasoning_tokens": 0
        },
        "actual_credits_micro": 0,
        "settlement_method": "none",
        "policy_version_applied": 0,
        "web_search_calls": 0,
        "code_interpreter_calls": 0,
        "file_search_calls": 0,
        "timestamp": "2026-09-26T12:00:00Z",
        "requester_type": "system",
        "dedupe_key": format!("{tenant_hex}/thread_summary_update/{system_request_hex}"),
        "system_task_type": "thread_summary_update"
    });

    let event: UsageEvent = serde_json::from_value(payload).expect("design payload must parse");

    assert!(event.user_id.is_none());
    assert!(event.turn_id.is_none());
    assert_eq!(event.chat_id, Some(parse_uuid(CHAT)));
    assert_eq!(
        event.system_task_type.as_deref(),
        Some("thread_summary_update")
    );
    let usage = event.usage.as_ref().expect("usage is present");
    assert_eq!(usage.input_tokens, 5_000);
    assert_eq!(usage.cache_read_input_tokens, 4_000);
    let expected_timestamp =
        OffsetDateTime::parse("2026-09-26T12:00:00Z", &Rfc3339).expect("valid RFC 3339");
    assert_eq!(event.timestamp, expected_timestamp);
}

#[test]
fn usage_event_round_trip_preserves_optional_ids() {
    let original = user_turn_event();

    let encoded = serde_json::to_value(&original).expect("event must serialize");
    let decoded: UsageEvent = serde_json::from_value(encoded).expect("event must deserialize");

    assert_eq!(decoded.user_id, original.user_id);
    assert_eq!(decoded.turn_id, original.turn_id);
    assert_eq!(decoded.chat_id, original.chat_id);
    assert!(decoded.usage.is_none());
    assert_eq!(decoded.dedupe_key, original.dedupe_key);
    assert_eq!(decoded.timestamp, original.timestamp);
}

#[test]
fn usage_tokens_default_is_all_zero() {
    let tokens = UsageTokens::default();

    assert_eq!(tokens.input_tokens, 0);
    assert_eq!(tokens.output_tokens, 0);
    assert_eq!(tokens.cache_read_input_tokens, 0);
    assert_eq!(tokens.cache_write_input_tokens, 0);
    assert_eq!(tokens.reasoning_tokens, 0);
}

// ── Dedupe keys ──────────────────────────────────────────────────────────

#[test]
fn dedupe_key_for_turn_uses_simple_lowercase_hex_parts() {
    let key = dedupe_key_for_turn(parse_uuid(TENANT), parse_uuid(TURN), parse_uuid(REQUEST));

    assert_eq!(
        key,
        "f47ac10b58cc4372a5670e02b2c3d479/0b6e0f6a1c2d4e3f8a9b0c1d2e3f4a5b/9a8b7c6d5e4f4a3b9c2d1e0f9a8b7c6d"
    );
}

#[test]
fn dedupe_key_for_turn_lowercases_uppercase_uuid_input() {
    let upper_tenant = parse_uuid(&TENANT.to_uppercase());

    let key = dedupe_key_for_turn(upper_tenant, parse_uuid(TURN), parse_uuid(REQUEST));

    assert!(
        key.starts_with("f47ac10b58cc4372a5670e02b2c3d479/"),
        "key: {key}"
    );
    assert_eq!(key.len(), 32 * 3 + 2);
}

#[test]
fn dedupe_key_for_system_task_embeds_task_type() {
    let key = dedupe_key_for_system_task(
        parse_uuid(TENANT),
        "thread_summary_update",
        parse_uuid(REQUEST),
    );

    assert_eq!(
        key,
        "f47ac10b58cc4372a5670e02b2c3d479/thread_summary_update/9a8b7c6d5e4f4a3b9c2d1e0f9a8b7c6d"
    );
}
