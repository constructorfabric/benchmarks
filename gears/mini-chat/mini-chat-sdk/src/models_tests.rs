#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    KillSwitches, MiniChatAuditEvent, ModelCatalogEntry, ModelTier, TurnMutationAuditEvent,
    UsageEvent, UsageTokens,
};

fn minimal_entry() -> Value {
    json!({
        "id": "gpt-5.2",
        "provider_model_id": "gpt-5.2-2026-01-01",
        "display_name": "GPT 5.2",
        "provider_id": "openai",
        "provider_display_name": "OpenAI",
        "tier": "premium",
        "context_window": 128_000,
        "max_output_tokens": 16_000,
        "max_input_tokens": 100_000,
        "input_tokens_credit_multiplier_micro": 1_000_000,
        "output_tokens_credit_multiplier_micro": 3_000_000,
        "max_num_results": 5,
        "general_config": {
            "type": "chat",
            "available_from": "2026-01-01",
            "max_file_size_mb": 25,
            "api_params": {},
            "features": { "streaming": true, "structured_output": true },
            "tool_support": {
                "web_search": true, "file_search": true, "image_generation": false,
                "code_interpreter": true, "mcp": false
            },
            "supported_endpoints": {
                "chat_completions": true, "responses": true, "embeddings": false,
                "image_generation": false, "audio_speech_generation": false,
                "audio_transcription": false, "audio_translation": false
            }
        }
    })
}

#[test]
fn catalog_entry_minimal_json_applies_defaults() {
    let entry: ModelCatalogEntry = serde_json::from_value(minimal_entry()).unwrap();
    assert!(!entry.enabled);
    assert_eq!(entry.max_tool_calls, 2);
    assert_eq!(entry.web_search_context_size, "low");
    assert_eq!(entry.estimation_budgets.bytes_per_token_conservative, 4);
    assert_eq!(entry.estimation_budgets.minimal_generation_floor, 50);
    assert!(entry.preference.is_none());
}

#[test]
fn catalog_entry_missing_required_field_fails() {
    let mut v = minimal_entry();
    v.as_object_mut().unwrap().remove("max_num_results");
    assert!(serde_json::from_value::<ModelCatalogEntry>(v).is_err());
}

#[test]
fn tier_accepts_capitalized_and_serializes_lowercase() {
    let a: ModelTier = serde_json::from_value(json!("Premium")).unwrap();
    let b: ModelTier = serde_json::from_value(json!("premium")).unwrap();
    assert_eq!(a, ModelTier::Premium);
    assert_eq!(b, ModelTier::Premium);
    assert_eq!(serde_json::to_value(a).unwrap(), json!("premium"));
    let s: ModelTier = serde_json::from_value(json!("Standard")).unwrap();
    assert_eq!(serde_json::to_value(s).unwrap(), json!("standard"));
}

#[test]
fn catalog_entry_ignores_unknown_keys() {
    let mut v = minimal_entry();
    v.as_object_mut().unwrap().insert("foo".into(), json!(1));
    assert!(serde_json::from_value::<ModelCatalogEntry>(v).is_ok());
}

#[test]
fn kill_switches_require_every_field() {
    let full = json!({
        "disable_premium_tier": false,
        "force_standard_tier": false,
        "disable_web_search": false,
        "disable_file_search": false,
        "disable_images": false,
        "disable_code_interpreter": false
    });
    assert!(serde_json::from_value::<KillSwitches>(full.clone()).is_ok());
    let mut v = full;
    v.as_object_mut().unwrap().remove("disable_images");
    assert!(serde_json::from_value::<KillSwitches>(v).is_err());
}

fn base_usage_event() -> UsageEvent {
    UsageEvent {
        tenant_id: Uuid::from_u128(1),
        user_id: Some(Uuid::from_u128(2)),
        chat_id: Uuid::from_u128(3),
        turn_id: Some(Uuid::from_u128(4)),
        request_id: Uuid::from_u128(5),
        effective_model: "gpt-5.2".into(),
        selected_model: "gpt-5.2".into(),
        terminal_state: "completed".into(),
        billing_outcome: "completed".into(),
        usage: Some(UsageTokens {
            input_tokens: 10,
            output_tokens: 20,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_tokens: 0,
        }),
        actual_credits_micro: 100,
        settlement_method: "actual".into(),
        policy_version_applied: 7,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        file_search_calls: 0,
        timestamp: "2026-09-26T12:00:00Z".into(),
        requester_type: "user".into(),
        dedupe_key: "k".into(),
        system_task_type: None,
    }
}

#[test]
fn usage_event_system_task_omits_user_and_turn() {
    let mut ev = base_usage_event();
    ev.user_id = None;
    ev.turn_id = None;
    ev.system_task_type = Some("thread_summary_update".into());
    let v = serde_json::to_value(&ev).unwrap();
    let obj = v.as_object().unwrap();
    assert!(!obj.contains_key("user_id"));
    assert!(!obj.contains_key("turn_id"));
    assert_eq!(obj["system_task_type"], json!("thread_summary_update"));

    let mut user = base_usage_event();
    user.usage = None;
    let v = serde_json::to_value(&user).unwrap();
    let obj = v.as_object().unwrap();
    assert_eq!(obj["usage"], Value::Null);
    assert!(!obj.contains_key("system_task_type"));
    assert!(obj.contains_key("user_id"));
}

#[test]
fn audit_event_tagged_by_event_type() {
    let ev = MiniChatAuditEvent::Mutation(TurnMutationAuditEvent::Retry {
        tenant_id: Uuid::from_u128(1),
        actor_user_id: Uuid::from_u128(2),
        chat_id: Uuid::from_u128(3),
        original_request_id: Uuid::from_u128(4),
        new_request_id: Uuid::from_u128(5),
        timestamp: "2026-09-26T12:00:00Z".into(),
    });
    let v = serde_json::to_value(&ev).unwrap();
    assert_eq!(v["event_type"], json!("turn_retry"));
    assert_eq!(v["original_request_id"], json!(Uuid::from_u128(4)));
    assert_eq!(v["new_request_id"], json!(Uuid::from_u128(5)));
    assert_eq!(ev.tenant_id(), Uuid::from_u128(1));
}
