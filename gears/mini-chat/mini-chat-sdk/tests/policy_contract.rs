//! Serde contract of the SDK policy types: `KillSwitches`, `TierLimits`,
//! `UserLimits`, `PolicySnapshot`, `PolicyVersionInfo`.
//!
//! Source of truth: DESIGN 5.2.1 / 5.2.6, Appendix A.1 (`GetPolicySnapshot`,
//! `GetUserLimits`) and Appendix B.1 ("the SDK `KillSwitches` type requires
//! every field, so a snapshot from another policy plugin with a missing or
//! renamed switch fails to deserialize instead of reading it as `false`";
//! "unknown keys inside a catalog entry or a limits object are ignored").

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::use_debug,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value
)]

use mini_chat_sdk::{KillSwitches, PolicySnapshot, PolicyVersionInfo, TierLimits, UserLimits};
use serde_json::{Value, json};
use uuid::Uuid;

const SWITCHES: [&str; 6] = [
    "disable_premium_tier",
    "force_standard_tier",
    "disable_web_search",
    "disable_file_search",
    "disable_images",
    "disable_code_interpreter",
];

fn all_switches(value: bool) -> Value {
    let mut m = serde_json::Map::new();
    for name in SWITCHES {
        m.insert(name.to_owned(), json!(value));
    }
    Value::Object(m)
}

fn catalog_entry_json(id: &str) -> Value {
    json!({
        "id": id,
        "provider_model_id": id,
        "display_name": id,
        "provider_id": "openai",
        "provider_display_name": "OpenAI",
        "tier": "standard",
        "enabled": true,
        "context_window": 128_000,
        "max_output_tokens": 4096,
        "max_input_tokens": 120_000,
        "input_tokens_credit_multiplier_micro": 1_000_000,
        "output_tokens_credit_multiplier_micro": 3_000_000,
        "max_num_results": 5,
        "general_config": {
            "type": "model.general.v1",
            "available_from": "2026-01-01T00:00:00Z",
            "max_file_size_mb": 25,
            "api_params": {
                "temperature": 0.7, "top_p": 1.0, "frequency_penalty": 0.0,
                "presence_penalty": 0.0, "stop": []
            },
            "features": {"streaming": true, "structured_output": false},
            "tool_support": {
                "web_search": true, "file_search": true, "image_generation": false,
                "code_interpreter": true, "mcp": false
            },
            "supported_endpoints": {
                "chat_completions": false, "responses": true, "embeddings": false,
                "image_generation": false, "audio_speech_generation": false,
                "audio_transcription": false, "audio_translation": false
            }
        }
    })
}

#[test]
fn kill_switches_deserialize_from_a_complete_object() {
    let mut v = all_switches(false);
    v["disable_web_search"] = json!(true);
    v["force_standard_tier"] = json!(true);
    let ks: KillSwitches = serde_json::from_value(v).expect("complete object parses");
    assert!(ks.disable_web_search);
    assert!(ks.force_standard_tier);
    assert!(!ks.disable_premium_tier);
    assert!(!ks.disable_file_search);
    assert!(!ks.disable_images);
    assert!(!ks.disable_code_interpreter);
}

#[test]
fn kill_switches_require_every_field_when_deserialized_as_sdk_type() {
    for missing in SWITCHES {
        let mut v = all_switches(false);
        v.as_object_mut().unwrap().remove(missing);
        let r = serde_json::from_value::<KillSwitches>(v);
        assert!(
            r.is_err(),
            "KillSwitches without `{missing}` must fail instead of reading as false"
        );
    }
    assert!(serde_json::from_value::<KillSwitches>(json!({})).is_err());
}

#[test]
fn kill_switches_round_trip() {
    let mut v = all_switches(false);
    v["disable_images"] = json!(true);
    let ks: KillSwitches = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(serde_json::to_value(&ks).unwrap(), v);
}

#[test]
fn policy_snapshot_parses_catalog_and_kill_switches() {
    let snap: PolicySnapshot = serde_json::from_value(json!({
        "policy_version": 12345,
        "model_catalog": [catalog_entry_json("gpt-a"), catalog_entry_json("gpt-b")],
        "kill_switches": all_switches(false),
    }))
    .expect("snapshot parses");

    assert_eq!(snap.policy_version, 12345);
    let ids: Vec<&str> = snap.model_catalog.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, vec!["gpt-a", "gpt-b"]);
    assert!(!snap.kill_switches.disable_premium_tier);
}

#[test]
fn policy_snapshot_with_a_partial_kill_switch_object_is_rejected() {
    let mut switches = all_switches(false);
    switches.as_object_mut().unwrap().remove("disable_images");
    let r = serde_json::from_value::<PolicySnapshot>(json!({
        "policy_version": 1,
        "model_catalog": [],
        "kill_switches": switches,
    }));
    assert!(r.is_err(), "a snapshot with a missing switch must not load");
}

#[test]
fn policy_snapshot_requires_policy_version_catalog_and_switches() {
    for missing in ["policy_version", "model_catalog", "kill_switches"] {
        let mut v = json!({
            "policy_version": 1,
            "model_catalog": [],
            "kill_switches": all_switches(false),
        });
        v.as_object_mut().unwrap().remove(missing);
        assert!(
            serde_json::from_value::<PolicySnapshot>(v).is_err(),
            "snapshot without `{missing}` must not deserialize"
        );
    }
}

#[test]
fn policy_snapshot_round_trips() {
    let snap: PolicySnapshot = serde_json::from_value(json!({
        "policy_version": 7,
        "model_catalog": [catalog_entry_json("gpt-a")],
        "kill_switches": all_switches(true),
    }))
    .unwrap();
    let again: PolicySnapshot =
        serde_json::from_value(serde_json::to_value(&snap).unwrap()).unwrap();
    assert_eq!(snap, again);
}

#[test]
fn tier_limits_use_micro_credit_field_names_and_ignore_unknown_keys() {
    let t: TierLimits = serde_json::from_value(json!({
        "limit_daily_credits_micro": 120_000_000_u64,
        "limit_monthly_credits_micro": 2_500_000_000_u64,
        "limit_4h_credits_micro": 5,
    }))
    .expect("unknown keys inside TierLimits are ignored");
    assert_eq!(t.limit_daily_credits_micro, 120_000_000);
    assert_eq!(t.limit_monthly_credits_micro, 2_500_000_000);
}

#[test]
fn tier_limits_require_both_periods() {
    assert!(serde_json::from_value::<TierLimits>(json!({"limit_daily_credits_micro": 1})).is_err());
    assert!(
        serde_json::from_value::<TierLimits>(json!({"limit_monthly_credits_micro": 1})).is_err()
    );
}

#[test]
fn user_limits_match_the_get_user_limits_response() {
    let user = Uuid::new_v4();
    let limits: UserLimits = serde_json::from_value(json!({
        "user_id": user,
        "policy_version": 12345,
        "standard": {
            "limit_daily_credits_micro": 120_000_000_u64,
            "limit_monthly_credits_micro": 2_500_000_000_u64
        },
        "premium": {
            "limit_daily_credits_micro": 40_000_000_u64,
            "limit_monthly_credits_micro": 800_000_000_u64
        }
    }))
    .expect("user limits parse");

    assert_eq!(limits.user_id, user);
    assert_eq!(limits.policy_version, 12345);
    assert_eq!(limits.standard.limit_daily_credits_micro, 120_000_000);
    assert_eq!(limits.standard.limit_monthly_credits_micro, 2_500_000_000);
    assert_eq!(limits.premium.limit_daily_credits_micro, 40_000_000);
    assert_eq!(limits.premium.limit_monthly_credits_micro, 800_000_000);

    let again: UserLimits = serde_json::from_value(serde_json::to_value(&limits).unwrap()).unwrap();
    assert_eq!(limits, again);
}

#[test]
fn user_limits_have_no_four_hour_field() {
    // DESIGN 5.2.1: "There is no 4-hour limit field (4h periods are P2+)".
    let user = Uuid::new_v4();
    let limits = UserLimits {
        user_id: user,
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
    let v = serde_json::to_value(&limits).unwrap();
    let rendered = v.to_string();
    assert!(!rendered.contains("4h"), "unexpected 4h field: {rendered}");
    assert_eq!(
        v["standard"].as_object().unwrap().len(),
        2,
        "TierLimits carries exactly the daily and monthly limits"
    );
}

#[test]
fn policy_version_info_exposes_the_version() {
    let info: PolicyVersionInfo = serde_json::from_value(json!({
        "policy_version": 12345,
        "generated_at": "2026-02-27T13:00:00Z"
    }))
    .expect("GetCurrentPolicyVersion response parses");
    assert_eq!(info.policy_version, 12345);
}
