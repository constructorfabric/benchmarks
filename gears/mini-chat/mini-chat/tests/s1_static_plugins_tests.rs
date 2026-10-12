#![allow(clippy::unwrap_used, clippy::expect_used)]

//! S1 bundled static plugin tests (DESIGN Appendix B.1 "Bundled plugins",
//! section 5.2 policy plugin contract, section 5.3 multiplier bounds).

use mini_chat::infra::plugins::static_audit::StaticAuditConfig;
use mini_chat::infra::plugins::static_model_policy::{
    StaticModelPolicyConfig, StaticModelPolicyService,
};
use mini_chat_sdk::{
    KillSwitches, MiniChatModelPolicyPluginClientV1, ModelTier, UsageEvent, UsageTokens,
    dedupe_key_for_turn,
};
use serde_json::{Value, json};
use time::OffsetDateTime;
use uuid::Uuid;

const MAX_MULTIPLIER: i64 = 10_000_000_000;

fn merge(target: &mut Value, patch: Value) {
    match (target, patch) {
        (Value::Object(dst), Value::Object(src)) => {
            for (key, value) in src {
                merge(dst.entry(key).or_insert(Value::Null), value);
            }
        }
        (slot, value) => *slot = value,
    }
}

fn catalog_entry(id: &str, tier: &str, in_mult: i64, out_mult: i64) -> Value {
    json!({
        "id": id,
        "provider_model_id": id,
        "display_name": id,
        "provider_id": "azure_openai",
        "tier": tier,
        "enabled": true,
        "context_window": 1_047_576,
        "max_output_tokens": 32_768,
        "input_tokens_credit_multiplier_micro": in_mult,
        "output_tokens_credit_multiplier_micro": out_mult
    })
}

fn yaml_entry_tail() -> Value {
    json!({
        "description": "",
        "provider_display_name": "Azure OpenAI",
        "icon": "",
        "system_prompt": "You are a helpful, concise assistant. Answer clearly and directly.",
        "thread_summary_prompt": "",
        "multimodal_capabilities": ["VISION_INPUT"],
        "max_input_tokens": 1_047_576,
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
            "features": { "streaming": true, "structured_output": true },
            "tool_support": {
                "web_search": false,
                "file_search": false,
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
        }
    })
}

/// The `static-mini-chat-model-policy-plugin.config` section of
/// `config/mini-chat.yaml`, as JSON.
fn yaml_policy_section() -> Value {
    let mut premium = catalog_entry("gpt-4.1", "Premium", 3_000_000, 15_000_000);
    merge(&mut premium, yaml_entry_tail());
    merge(
        &mut premium,
        json!({
            "description": "Most capable model",
            "multiplier_display": "3x",
            "general_config": { "tool_support": { "web_search": true, "file_search": true } },
            "preference": { "is_default": true, "sort_order": 0 }
        }),
    );

    let mut standard = catalog_entry("gpt-4.1-mini", "Standard", 1_000_000, 3_000_000);
    merge(&mut standard, yaml_entry_tail());
    merge(
        &mut standard,
        json!({
            "description": "Fast and efficient model",
            "multiplier_display": "1x",
            "preference": { "is_default": false, "sort_order": 1 }
        }),
    );

    json!({
        "vendor": "constructorfabric",
        "priority": 100,
        "model_catalog": [premium, standard]
    })
}

fn parse_policy(raw: Value) -> Result<StaticModelPolicyConfig, serde_json::Error> {
    serde_json::from_value(raw)
}

fn policy(raw: Value) -> StaticModelPolicyConfig {
    parse_policy(raw).expect("policy plugin config must deserialize")
}

/// A one-entry catalog whose entry is patched with `entry_patch`.
fn policy_with_entry_patch(entry_patch: Value) -> StaticModelPolicyConfig {
    let mut entry = catalog_entry("m-1", "Standard", 1_000_000, 3_000_000);
    merge(&mut entry, entry_patch);
    policy(json!({ "model_catalog": [entry] }))
}

fn service(raw: Value) -> StaticModelPolicyService {
    StaticModelPolicyService::new(policy(raw))
}

fn user() -> Uuid {
    Uuid::parse_str("11111111-2222-4333-8444-555555555555").expect("valid UUID literal")
}

fn sample_usage_event() -> UsageEvent {
    let tenant_id = Uuid::parse_str("f47ac10b-58cc-4372-a567-0e02b2c3d479").expect("valid UUID");
    let turn_id = Uuid::parse_str("0b6e0f6a-1c2d-4e3f-8a9b-0c1d2e3f4a5b").expect("valid UUID");
    let request_id = Uuid::parse_str("9a8b7c6d-5e4f-4a3b-9c2d-1e0f9a8b7c6d").expect("valid UUID");
    UsageEvent {
        tenant_id,
        user_id: Some(user()),
        chat_id: None,
        turn_id: Some(turn_id),
        request_id,
        effective_model: "gpt-4.1".to_owned(),
        selected_model: "gpt-4.1".to_owned(),
        terminal_state: "completed".to_owned(),
        billing_outcome: "completed".to_owned(),
        usage: Some(UsageTokens {
            input_tokens: 1000,
            output_tokens: 500,
            ..UsageTokens::default()
        }),
        actual_credits_micro: 10_500,
        settlement_method: "actual".to_owned(),
        policy_version_applied: 1,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        file_search_calls: 0,
        timestamp: OffsetDateTime::UNIX_EPOCH,
        requester_type: "user".to_owned(),
        dedupe_key: dedupe_key_for_turn(tenant_id, turn_id, request_id),
        system_task_type: None,
    }
}

// ── StaticModelPolicyConfig: parsing ─────────────────────────────────────

#[test]
fn policy_config_yaml_section_deserializes_and_validates() {
    let cfg = policy(yaml_policy_section());

    assert_eq!(cfg.vendor, "constructorfabric");
    assert_eq!(cfg.priority, 100);
    assert_eq!(cfg.model_catalog.len(), 2);
    assert_eq!(cfg.model_catalog[0].id, "gpt-4.1");
    assert_eq!(cfg.model_catalog[0].tier, ModelTier::Premium);
    assert_eq!(cfg.model_catalog[1].id, "gpt-4.1-mini");
    assert_eq!(cfg.model_catalog[1].tier, ModelTier::Standard);
    assert_eq!(cfg.validate(), Ok(()));
}

#[test]
fn policy_config_without_model_catalog_is_rejected() {
    let result = parse_policy(json!({ "vendor": "constructorfabric", "priority": 100 }));

    assert!(
        result.is_err(),
        "`model_catalog` is required when the section is present"
    );
}

#[test]
fn policy_config_empty_catalog_is_valid() {
    let cfg = policy(json!({ "model_catalog": [] }));

    assert!(cfg.model_catalog.is_empty());
    assert_eq!(cfg.validate(), Ok(()));
}

#[test]
fn policy_config_rejects_unknown_top_level_key() {
    let result = parse_policy(json!({ "model_catalog": [], "default_limits": {} }));

    assert!(result.is_err());
}

#[test]
fn policy_config_defaults_vendor_priority_and_limits() {
    let cfg = policy(json!({ "model_catalog": [] }));

    assert_eq!(cfg.vendor, "constructorfabric");
    assert_eq!(cfg.priority, 100);
    assert_eq!(
        cfg.default_standard_limits.limit_daily_credits_micro,
        100_000_000
    );
    assert_eq!(
        cfg.default_standard_limits.limit_monthly_credits_micro,
        1_000_000_000
    );
    assert_eq!(
        cfg.default_premium_limits.limit_daily_credits_micro,
        50_000_000
    );
    assert_eq!(
        cfg.default_premium_limits.limit_monthly_credits_micro,
        500_000_000
    );
}

#[test]
fn policy_config_default_impl_matches_absent_section_defaults() {
    let cfg = StaticModelPolicyConfig::default();

    assert_eq!(cfg.vendor, "constructorfabric");
    assert_eq!(cfg.priority, 100);
    assert!(cfg.model_catalog.is_empty());
    assert_eq!(
        cfg.default_standard_limits.limit_daily_credits_micro,
        100_000_000
    );
    assert_eq!(
        cfg.default_premium_limits.limit_monthly_credits_micro,
        500_000_000
    );
    assert_eq!(cfg.validate(), Ok(()));
}

#[test]
fn policy_config_limits_object_ignores_unknown_keys() {
    let cfg = policy(json!({
        "model_catalog": [],
        "default_standard_limits": {
            "limit_daily_credits_micro": 7,
            "limit_monthly_credits_micro": 70,
            "future_limit": 1
        }
    }));

    assert_eq!(cfg.default_standard_limits.limit_daily_credits_micro, 7);
    assert_eq!(cfg.default_standard_limits.limit_monthly_credits_micro, 70);
}

// ── StaticModelPolicyConfig: kill switches ───────────────────────────────

#[test]
fn policy_config_kill_switches_default_to_false_when_omitted() {
    let cfg = policy(json!({ "model_catalog": [] }));
    let switches = &cfg.kill_switches;

    assert!(!switches.disable_premium_tier);
    assert!(!switches.force_standard_tier);
    assert!(!switches.disable_web_search);
    assert!(!switches.disable_file_search);
    assert!(!switches.disable_images);
    assert!(!switches.disable_code_interpreter);
}

#[test]
fn policy_config_kill_switches_missing_fields_default_to_false() {
    let cfg = policy(json!({
        "model_catalog": [],
        "kill_switches": { "disable_web_search": true }
    }));
    let switches = &cfg.kill_switches;

    assert!(switches.disable_web_search);
    assert!(!switches.disable_premium_tier);
    assert!(!switches.force_standard_tier);
    assert!(!switches.disable_file_search);
    assert!(!switches.disable_images);
    assert!(!switches.disable_code_interpreter);
}

#[test]
fn policy_config_kill_switches_reject_unknown_key() {
    let result = parse_policy(json!({
        "model_catalog": [],
        "kill_switches": { "disable_websearch": true }
    }));

    assert!(
        result.is_err(),
        "a misspelled kill switch must fail plugin init"
    );
}

#[test]
fn policy_config_kill_switches_convert_into_sdk_type() {
    let cfg = policy(json!({
        "model_catalog": [],
        "kill_switches": {
            "disable_premium_tier": true,
            "force_standard_tier": false,
            "disable_web_search": true,
            "disable_file_search": false,
            "disable_images": true,
            "disable_code_interpreter": false
        }
    }));

    let switches: KillSwitches = cfg.kill_switches.into();

    assert!(switches.disable_premium_tier);
    assert!(!switches.force_standard_tier);
    assert!(switches.disable_web_search);
    assert!(!switches.disable_file_search);
    assert!(switches.disable_images);
    assert!(!switches.disable_code_interpreter);
}

// ── StaticModelPolicyConfig: validate() ──────────────────────────────────

#[test]
fn policy_validate_accepts_multiplier_bounds() {
    for (in_mult, out_mult) in [(1, 1), (MAX_MULTIPLIER, MAX_MULTIPLIER)] {
        let cfg = policy_with_entry_patch(json!({
            "input_tokens_credit_multiplier_micro": in_mult,
            "output_tokens_credit_multiplier_micro": out_mult
        }));

        assert_eq!(
            cfg.validate(),
            Ok(()),
            "multipliers ({in_mult}, {out_mult}) must be accepted"
        );
    }
}

#[test]
fn policy_validate_rejects_zero_multiplier() {
    for field in [
        "input_tokens_credit_multiplier_micro",
        "output_tokens_credit_multiplier_micro",
    ] {
        let mut patch = serde_json::Map::new();
        patch.insert(field.to_owned(), json!(0));
        let cfg = policy_with_entry_patch(Value::Object(patch));

        assert!(cfg.validate().is_err(), "{field} = 0 must fail plugin init");
    }
}

#[test]
fn policy_validate_rejects_negative_multiplier() {
    for field in [
        "input_tokens_credit_multiplier_micro",
        "output_tokens_credit_multiplier_micro",
    ] {
        let mut patch = serde_json::Map::new();
        patch.insert(field.to_owned(), json!(-1));
        let cfg = policy_with_entry_patch(Value::Object(patch));

        assert!(
            cfg.validate().is_err(),
            "{field} = -1 must fail plugin init"
        );
    }
}

#[test]
fn policy_validate_rejects_multiplier_above_max() {
    for field in [
        "input_tokens_credit_multiplier_micro",
        "output_tokens_credit_multiplier_micro",
    ] {
        let mut patch = serde_json::Map::new();
        patch.insert(field.to_owned(), json!(MAX_MULTIPLIER + 1));
        let cfg = policy_with_entry_patch(Value::Object(patch));

        assert!(
            cfg.validate().is_err(),
            "{field} = 10_000_000_001 must fail plugin init"
        );
    }
}

#[test]
fn policy_validate_rejects_invalid_entry_among_valid_ones() {
    let mut raw = yaml_policy_section();
    let catalog = raw
        .get_mut("model_catalog")
        .and_then(Value::as_array_mut)
        .expect("catalog is an array");
    catalog.push(catalog_entry("free-model", "Standard", 0, 1_000_000));
    let cfg = policy(raw);

    assert!(cfg.validate().is_err());
}

#[test]
fn policy_validate_rejects_zero_bytes_per_token() {
    let cfg = policy_with_entry_patch(json!({
        "estimation_budgets": { "bytes_per_token_conservative": 0 }
    }));

    assert!(cfg.validate().is_err());
}

#[test]
fn policy_validate_accepts_omitted_estimation_budgets() {
    let cfg = policy_with_entry_patch(json!({}));

    assert_eq!(cfg.validate(), Ok(()));
}

// ── StaticModelPolicyService ─────────────────────────────────────────────

#[tokio::test]
async fn policy_service_current_version_is_one() {
    let svc = service(yaml_policy_section());

    let version = svc
        .get_current_policy_version(user())
        .await
        .expect("version must be served");

    assert_eq!(version, 1);
}

#[tokio::test]
async fn policy_service_snapshot_carries_configured_catalog() {
    let svc = service(yaml_policy_section());

    let snapshot = svc
        .get_policy_snapshot(user(), 1)
        .await
        .expect("snapshot for version 1 must be served");

    assert_eq!(snapshot.policy_version, 1);
    let ids: Vec<&str> = snapshot
        .model_catalog
        .iter()
        .map(|e| e.id.as_str())
        .collect();
    assert_eq!(ids, ["gpt-4.1", "gpt-4.1-mini"]);
    assert_eq!(
        snapshot.model_catalog[0].input_tokens_credit_multiplier_micro,
        3_000_000
    );
    assert_eq!(
        snapshot.model_catalog[0].output_tokens_credit_multiplier_micro,
        15_000_000
    );
}

#[tokio::test]
async fn policy_service_snapshot_carries_configured_kill_switches() {
    let mut raw = yaml_policy_section();
    merge(
        &mut raw,
        json!({ "kill_switches": { "disable_premium_tier": true, "disable_images": true } }),
    );
    let svc = service(raw);

    let snapshot = svc
        .get_policy_snapshot(user(), 1)
        .await
        .expect("snapshot for version 1 must be served");
    let switches = &snapshot.kill_switches;

    assert!(switches.disable_premium_tier);
    assert!(switches.disable_images);
    assert!(!switches.force_standard_tier);
    assert!(!switches.disable_web_search);
    assert!(!switches.disable_file_search);
    assert!(!switches.disable_code_interpreter);
}

#[tokio::test]
async fn policy_service_snapshot_for_unknown_version_fails() {
    let svc = service(yaml_policy_section());

    for version in [0, 2, u64::MAX] {
        let result = svc.get_policy_snapshot(user(), version).await;

        assert!(result.is_err(), "version {version} must not be served");
    }
}

#[tokio::test]
async fn policy_service_user_limits_echo_user_and_version() {
    let svc = service(yaml_policy_section());
    let user_id = user();

    let limits = svc
        .get_user_limits(user_id, 1)
        .await
        .expect("limits must be served");

    assert_eq!(limits.user_id, user_id);
    assert_eq!(limits.policy_version, 1);
}

#[tokio::test]
async fn policy_service_user_limits_use_default_tier_limits() {
    let svc = service(yaml_policy_section());

    let limits = svc
        .get_user_limits(user(), 1)
        .await
        .expect("limits must be served");

    assert_eq!(limits.standard.limit_daily_credits_micro, 100_000_000);
    assert_eq!(limits.standard.limit_monthly_credits_micro, 1_000_000_000);
    assert_eq!(limits.premium.limit_daily_credits_micro, 50_000_000);
    assert_eq!(limits.premium.limit_monthly_credits_micro, 500_000_000);
}

#[tokio::test]
async fn policy_service_user_limits_use_configured_tier_limits_for_every_user() {
    let mut raw = yaml_policy_section();
    merge(
        &mut raw,
        json!({
            "default_standard_limits": {
                "limit_daily_credits_micro": 60_000_000,
                "limit_monthly_credits_micro": 600_000_000
            },
            "default_premium_limits": {
                "limit_daily_credits_micro": 22_000_000,
                "limit_monthly_credits_micro": 300_000_000
            }
        }),
    );
    let svc = service(raw);
    let other_user =
        Uuid::parse_str("22222222-3333-4444-8555-666666666666").expect("valid UUID literal");

    for user_id in [user(), other_user] {
        let limits = svc
            .get_user_limits(user_id, 1)
            .await
            .expect("limits must be served");

        assert_eq!(limits.user_id, user_id);
        assert_eq!(limits.standard.limit_daily_credits_micro, 60_000_000);
        assert_eq!(limits.standard.limit_monthly_credits_micro, 600_000_000);
        assert_eq!(limits.premium.limit_daily_credits_micro, 22_000_000);
        assert_eq!(limits.premium.limit_monthly_credits_micro, 300_000_000);
    }
}

#[tokio::test]
async fn policy_service_publish_usage_succeeds() {
    let svc = service(yaml_policy_section());

    let result = svc.publish_usage(sample_usage_event()).await;

    assert!(result.is_ok());
}

// ── StaticAuditConfig ────────────────────────────────────────────────────

#[test]
fn audit_config_empty_section_uses_defaults() {
    let cfg: StaticAuditConfig =
        serde_json::from_value(json!({})).expect("empty section must deserialize");

    assert!(cfg.enabled);
    assert_eq!(cfg.vendor, "constructorfabric");
    assert_eq!(cfg.priority, 100);
}

#[test]
fn audit_config_default_impl_matches_absent_section_defaults() {
    let cfg = StaticAuditConfig::default();

    assert!(cfg.enabled);
    assert_eq!(cfg.vendor, "constructorfabric");
    assert_eq!(cfg.priority, 100);
}

#[test]
fn audit_config_yaml_section_deserializes() {
    let cfg: StaticAuditConfig = serde_json::from_value(json!({
        "vendor": "constructorfabric",
        "priority": 100,
        "enabled": true
    }))
    .expect("yaml section must deserialize");

    assert!(cfg.enabled);
    assert_eq!(cfg.vendor, "constructorfabric");
    assert_eq!(cfg.priority, 100);
}

#[test]
fn audit_config_can_be_disabled() {
    let cfg: StaticAuditConfig =
        serde_json::from_value(json!({ "enabled": false })).expect("must deserialize");

    assert!(!cfg.enabled);
}

#[test]
fn audit_config_rejects_unknown_key() {
    let result = serde_json::from_value::<StaticAuditConfig>(json!({ "enable": true }));

    assert!(result.is_err());
}
