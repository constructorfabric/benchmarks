//! Contract tests of the SDK data types: serde shapes, defaults and the
//! unknown-key rules of DESIGN 5.2 and Appendix B.1.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use serde_json::{Value, json};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::models::{
    AuditEvent, EstimationBudgets, KillSwitches, ModelCatalogEntry, ModelTier, PolicySnapshot,
    QuotaDecision, TierLimits, TurnAuditEvent, TurnDeleteAuditEvent, TurnMutationAuditEvent,
    UsageEvent, UsageTokens, UserLimits, WebSearchContextSize,
};

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

/// A catalog entry with exactly the required keys of DESIGN 6619.
fn minimal_entry() -> Value {
    json!({
        "id": "gpt-5.2",
        "provider_model_id": "gpt-5.2-deploy",
        "display_name": "GPT-5.2",
        "provider_id": "azure_openai",
        "provider_display_name": "Azure OpenAI",
        "tier": "premium",
        "context_window": 128_000,
        "max_output_tokens": 4096,
        "max_input_tokens": 120_000,
        "input_tokens_credit_multiplier_micro": 2_500_000,
        "output_tokens_credit_multiplier_micro": 10_000_000,
        "max_num_results": 5,
        "general_config": general_config()
    })
}

fn all_switches() -> Value {
    json!({
        "disable_premium_tier": false,
        "force_standard_tier": false,
        "disable_web_search": false,
        "disable_file_search": false,
        "disable_images": false,
        "disable_code_interpreter": false
    })
}

#[test]
fn model_tier_serializes_lowercase() {
    assert_eq!(
        serde_json::to_value(ModelTier::Standard).unwrap(),
        json!("standard")
    );
    assert_eq!(
        serde_json::to_value(ModelTier::Premium).unwrap(),
        json!("premium")
    );
}

#[test]
fn model_tier_accepts_lowercase_and_capitalised_spelling() {
    let cases = vec![
        ("standard", ModelTier::Standard),
        ("premium", ModelTier::Premium),
        ("Standard", ModelTier::Standard),
        ("Premium", ModelTier::Premium),
    ];
    for (raw, expected) in cases {
        let got: ModelTier = serde_json::from_value(json!(raw)).unwrap();
        assert_eq!(got, expected, "{raw}");
    }
    assert!(serde_json::from_value::<ModelTier>(json!("gold")).is_err());
}

#[test]
fn catalog_entry_enabled_defaults_to_false_when_omitted() {
    let entry: ModelCatalogEntry = serde_json::from_value(minimal_entry()).unwrap();
    assert!(!entry.enabled);
}

#[test]
fn catalog_entry_applies_documented_defaults() {
    let entry: ModelCatalogEntry = serde_json::from_value(minimal_entry()).unwrap();
    assert_eq!(entry.description, None);
    assert_eq!(entry.icon, "");
    assert_eq!(entry.multiplier_display, "");
    assert!(entry.multimodal_capabilities.is_empty());
    assert_eq!(entry.estimation_budgets, EstimationBudgets::default());
    assert_eq!(entry.web_search_context_size, WebSearchContextSize::Low);
    assert_eq!(entry.max_tool_calls, 2);
    assert_eq!(entry.preference, None);
    assert_eq!(entry.system_prompt, "");
    assert_eq!(entry.thread_summary_prompt, "");
}

#[test]
fn catalog_entry_missing_required_key_fails_to_deserialize() {
    let required = [
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
    for key in required {
        let mut value = minimal_entry();
        value.as_object_mut().unwrap().remove(key);
        assert!(
            serde_json::from_value::<ModelCatalogEntry>(value).is_err(),
            "missing `{key}` must fail"
        );
    }
}

#[test]
fn catalog_entry_ignores_unknown_keys_including_nested() {
    let mut value = minimal_entry();
    value["brand_new_key"] = json!(1);
    value["general_config"]["brand_new_nested"] = json!({"a": 1});
    value["general_config"]["api_params"]["future_param"] = json!(true);
    value["estimation_budgets"] = json!({ "safety_margin_pct": 25, "future_budget": 7 });
    let entry: ModelCatalogEntry = serde_json::from_value(value).unwrap();
    assert_eq!(entry.estimation_budgets.safety_margin_pct, 25);
    assert_eq!(entry.estimation_budgets.bytes_per_token_conservative, 4);
}

#[test]
fn catalog_entry_general_config_requires_its_fields_except_two_api_params() {
    let mut value = minimal_entry();
    value["general_config"]["api_params"] = json!({ "stop": [], "temperature": 0.7 });
    assert!(serde_json::from_value::<ModelCatalogEntry>(value).is_ok());

    for key in [
        "max_file_size_mb",
        "features",
        "tool_support",
        "supported_endpoints",
        "api_params",
    ] {
        let mut value = minimal_entry();
        value["general_config"].as_object_mut().unwrap().remove(key);
        assert!(
            serde_json::from_value::<ModelCatalogEntry>(value).is_err(),
            "general_config without `{key}` must fail"
        );
    }
}

#[test]
fn catalog_entry_preference_and_description_roundtrip() {
    let mut value = minimal_entry();
    value["enabled"] = json!(true);
    value["description"] = json!("Best for complex reasoning tasks");
    value["multiplier_display"] = json!("2.5x");
    value["multimodal_capabilities"] = json!(["VISION_INPUT", "RAG"]);
    value["preference"] = json!({ "is_default": true, "sort_order": 3 });
    let entry: ModelCatalogEntry = serde_json::from_value(value).unwrap();
    assert!(entry.enabled);
    assert_eq!(
        entry.description.as_deref(),
        Some("Best for complex reasoning tasks")
    );
    assert_eq!(entry.multimodal_capabilities, vec!["VISION_INPUT", "RAG"]);
    let preference = entry.preference.clone().unwrap();
    assert!(preference.is_default);
    assert_eq!(preference.sort_order, 3);
    let again: ModelCatalogEntry =
        serde_json::from_value(serde_json::to_value(&entry).unwrap()).unwrap();
    assert_eq!(again, entry);
}

#[test]
fn estimation_budgets_defaults_match_appendix_b() {
    let defaults = EstimationBudgets::default();
    assert_eq!(defaults.bytes_per_token_conservative, 4);
    assert_eq!(defaults.fixed_overhead_tokens, 100);
    assert_eq!(defaults.safety_margin_pct, 10);
    assert_eq!(defaults.image_token_budget, 1000);
    assert_eq!(defaults.tool_surcharge_tokens, 500);
    assert_eq!(defaults.web_search_surcharge_tokens, 500);
    assert_eq!(defaults.code_interpreter_surcharge_tokens, 1000);
    assert_eq!(defaults.minimal_generation_floor, 50);
    let empty: EstimationBudgets = serde_json::from_value(json!({})).unwrap();
    assert_eq!(empty, defaults);
}

#[test]
fn kill_switches_require_every_field() {
    let all: KillSwitches = serde_json::from_value(all_switches()).unwrap();
    assert_eq!(all, KillSwitches::default());

    for key in [
        "disable_premium_tier",
        "force_standard_tier",
        "disable_web_search",
        "disable_file_search",
        "disable_images",
        "disable_code_interpreter",
    ] {
        let mut value = all_switches();
        value.as_object_mut().unwrap().remove(key);
        assert!(
            serde_json::from_value::<KillSwitches>(value).is_err(),
            "missing `{key}` must fail instead of reading false"
        );
    }
}

#[test]
fn kill_switches_reject_a_renamed_switch_snapshot() {
    let value = json!({
        "disable_premium_tier": false,
        "force_standard_tier": false,
        "disable_web_search": false,
        "disable_file_search": false,
        "disable_images": false,
        "disable_code_interpreter_v2": false
    });
    assert!(serde_json::from_value::<KillSwitches>(value).is_err());
}

#[test]
fn tier_limits_ignore_unknown_keys() {
    let limits: TierLimits = serde_json::from_value(json!({
        "limit_daily_credits_micro": 10,
        "limit_monthly_credits_micro": 20,
        "limit_4h_credits_micro": 5
    }))
    .unwrap();
    assert_eq!(limits.limit_daily_credits_micro, 10);
    assert_eq!(limits.limit_monthly_credits_micro, 20);
}

#[test]
fn user_limits_keep_standard_and_premium_apart_from_the_snapshot() {
    let snapshot = PolicySnapshot {
        policy_version: 1,
        model_catalog: vec![],
        kill_switches: KillSwitches::default(),
    };
    let rendered = serde_json::to_value(&snapshot).unwrap();
    assert!(rendered.get("user_limits").is_none());
    assert!(rendered.get("standard").is_none());

    let limits = UserLimits {
        user_id: Uuid::from_u128(7),
        policy_version: 1,
        standard: TierLimits {
            limit_daily_credits_micro: 1,
            limit_monthly_credits_micro: 2,
        },
        premium: TierLimits {
            limit_daily_credits_micro: 3,
            limit_monthly_credits_micro: 4,
        },
    };
    let again: UserLimits = serde_json::from_value(serde_json::to_value(limits).unwrap()).unwrap();
    assert_eq!(again, limits);
}

#[test]
fn policy_snapshot_roundtrips() {
    let entry: ModelCatalogEntry = serde_json::from_value(minimal_entry()).unwrap();
    let snapshot = PolicySnapshot {
        policy_version: 12_345,
        model_catalog: vec![entry],
        kill_switches: KillSwitches::default(),
    };
    let again: PolicySnapshot =
        serde_json::from_value(serde_json::to_value(&snapshot).unwrap()).unwrap();
    assert_eq!(again, snapshot);
}

fn usage_event(user: bool) -> UsageEvent {
    UsageEvent {
        tenant_id: Uuid::from_u128(1),
        user_id: user.then(|| Uuid::from_u128(2)),
        chat_id: Uuid::from_u128(3),
        turn_id: user.then(|| Uuid::from_u128(4)),
        request_id: Uuid::from_u128(5),
        effective_model: "m".to_owned(),
        selected_model: "m".to_owned(),
        terminal_state: "completed".to_owned(),
        billing_outcome: if user { "completed" } else { "system_task" }.to_owned(),
        usage: Some(UsageTokens {
            input_tokens: 5,
            output_tokens: 6,
            ..UsageTokens::default()
        }),
        actual_credits_micro: 0,
        settlement_method: if user { "actual" } else { "none" }.to_owned(),
        policy_version_applied: 12,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        file_search_calls: 1,
        timestamp: OffsetDateTime::UNIX_EPOCH,
        requester_type: if user { "user" } else { "system" }.to_owned(),
        dedupe_key: None,
        system_task_type: (!user).then(|| "thread_summary_update".to_owned()),
    }
}

#[test]
fn usage_event_omits_user_and_turn_ids_for_system_tasks() {
    let value = serde_json::to_value(usage_event(false)).unwrap();
    assert!(value.get("user_id").is_none());
    assert!(value.get("turn_id").is_none());
    assert_eq!(value["system_task_type"], "thread_summary_update");
    let user_turn = serde_json::to_value(usage_event(true)).unwrap();
    assert!(user_turn.get("user_id").is_some());
    assert!(user_turn.get("system_task_type").is_none());
}

#[test]
fn usage_event_timestamp_is_rfc3339_and_roundtrips() {
    let event = usage_event(true);
    let value = serde_json::to_value(&event).unwrap();
    assert_eq!(value["timestamp"], "1970-01-01T00:00:00Z");
    let again: UsageEvent = serde_json::from_value(value).unwrap();
    assert_eq!(again, event);
}

#[test]
fn usage_event_with_null_usage_roundtrips() {
    let mut event = usage_event(true);
    event.usage = None;
    let value = serde_json::to_value(&event).unwrap();
    assert!(value["usage"].is_null());
    let again: UsageEvent = serde_json::from_value(value).unwrap();
    assert_eq!(again.usage, None);
}

fn turn_audit() -> TurnAuditEvent {
    TurnAuditEvent {
        tenant_id: Uuid::from_u128(1),
        user_id: Uuid::from_u128(2),
        chat_id: Uuid::from_u128(3),
        turn_id: Uuid::from_u128(4),
        request_id: Uuid::from_u128(5),
        requester_type: "user".to_owned(),
        selected_model: "a".to_owned(),
        effective_model: "b".to_owned(),
        terminal_state: "completed".to_owned(),
        error_code: None,
        usage: None,
        latency_ms: 10,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        file_search_calls: 0,
        quota_decision: QuotaDecision::Downgrade,
        downgrade_from: Some("a".to_owned()),
        downgrade_reason: Some("premium_quota_exhausted".to_owned()),
        trace_id: None,
        timestamp: OffsetDateTime::UNIX_EPOCH,
        prompt: String::new(),
        response: String::new(),
        attachments: vec![],
        license: None,
        quota_scope: None,
    }
}

#[test]
fn audit_event_types_are_the_five_documented_values() {
    let at = OffsetDateTime::UNIX_EPOCH;
    let mutation = TurnMutationAuditEvent {
        actor_user_id: Uuid::from_u128(1),
        chat_id: Uuid::from_u128(2),
        original_request_id: Uuid::from_u128(3),
        new_request_id: Uuid::from_u128(4),
        timestamp: at,
    };
    let delete = TurnDeleteAuditEvent {
        actor_user_id: Uuid::from_u128(1),
        chat_id: Uuid::from_u128(2),
        request_id: Uuid::from_u128(3),
        timestamp: at,
    };
    let cases = vec![
        (AuditEvent::TurnCompleted(turn_audit()), "turn_completed"),
        (AuditEvent::TurnFailed(turn_audit()), "turn_failed"),
        (AuditEvent::TurnRetry(mutation.clone()), "turn_retry"),
        (AuditEvent::TurnEdit(mutation), "turn_edit"),
        (AuditEvent::TurnDelete(delete), "turn_delete"),
    ];
    for (event, tag) in cases {
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["event_type"], tag);
        let again: AuditEvent = serde_json::from_value(value).unwrap();
        assert_eq!(again, event);
    }
    let cancelled = json!({ "event_type": "turn_cancelled" });
    assert!(serde_json::from_value::<AuditEvent>(cancelled).is_err());
}

#[test]
fn turn_audit_event_content_fields_are_empty_by_default() {
    let mut value = serde_json::to_value(AuditEvent::TurnCompleted(turn_audit())).unwrap();
    for key in [
        "prompt",
        "response",
        "attachments",
        "license",
        "quota_scope",
    ] {
        value.as_object_mut().unwrap().remove(key);
    }
    let AuditEvent::TurnCompleted(event) = serde_json::from_value::<AuditEvent>(value).unwrap()
    else {
        panic!("wrong variant");
    };
    assert!(event.prompt.is_empty());
    assert!(event.response.is_empty());
    assert!(event.attachments.is_empty());
    assert_eq!(event.license, None);
    assert_eq!(event.quota_scope, None);
}
