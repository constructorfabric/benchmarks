#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::unreadable_literal,
    clippy::cognitive_complexity
)]

use mini_chat_sdk::MiniChatModelPolicyPluginClientV1;
use serde_json::json;
use uuid::Uuid;

use super::config::StaticModelPolicyConfig;
use super::*;

fn entry(input_mult: u64, output_mult: u64, bpt: u32) -> serde_json::Value {
    json!({
        "id": "m1",
        "provider_model_id": "m1",
        "display_name": "M1",
        "provider_id": "openai",
        "provider_display_name": "OpenAI",
        "tier": "Standard",
        "enabled": true,
        "context_window": 128000,
        "max_output_tokens": 4096,
        "max_input_tokens": 120000,
        "input_tokens_credit_multiplier_micro": input_mult,
        "output_tokens_credit_multiplier_micro": output_mult,
        "max_num_results": 5,
        "estimation_budgets": { "bytes_per_token_conservative": bpt },
        "general_config": {
            "type": "", "available_from": "1970-01-01T00:00:00Z", "max_file_size_mb": 25,
            "api_params": { "stop": [] },
            "features": { "streaming": true, "structured_output": false },
            "tool_support": { "web_search": false, "file_search": false, "image_generation": false,
                              "code_interpreter": false, "mcp": false },
            "supported_endpoints": { "chat_completions": true, "responses": true, "embeddings": false,
                                     "image_generation": false, "audio_speech_generation": false,
                                     "audio_transcription": false, "audio_translation": false }
        }
    })
}

fn cfg(v: serde_json::Value) -> StaticModelPolicyConfig {
    serde_json::from_value(v).unwrap()
}

#[test]
fn rejects_zero_multiplier() {
    let c = cfg(json!({ "model_catalog": [entry(0, 1_000_000, 4)] }));
    assert!(StaticModelPolicyService::from_config(&c).is_err());
    let c = cfg(json!({ "model_catalog": [entry(1_000_000, 0, 4)] }));
    assert!(StaticModelPolicyService::from_config(&c).is_err());
    let c = cfg(json!({ "model_catalog": [entry(10_000_000_001, 1, 4)] }));
    assert!(StaticModelPolicyService::from_config(&c).is_err());
    let c = cfg(json!({ "model_catalog": [entry(10_000_000_000, 1, 4)] }));
    assert!(StaticModelPolicyService::from_config(&c).is_ok());
}

#[test]
fn rejects_zero_bytes_per_token() {
    let c = cfg(json!({ "model_catalog": [entry(1, 1, 0)] }));
    assert!(StaticModelPolicyService::from_config(&c).is_err());
}

#[test]
fn rejects_unknown_kill_switch() {
    let r = serde_json::from_value::<StaticModelPolicyConfig>(json!({
        "model_catalog": [],
        "kill_switches": { "disable_everything": true }
    }));
    assert!(r.is_err());

    let c = cfg(json!({
        "model_catalog": [],
        "kill_switches": { "disable_web_search": true }
    }));
    let ks = mini_chat_sdk::KillSwitches::from(c.kill_switches);
    assert!(ks.disable_web_search);
    assert!(!ks.disable_premium_tier && !ks.force_standard_tier && !ks.disable_images);
}

#[test]
fn rejects_unknown_top_level_key_and_requires_catalog() {
    assert!(
        serde_json::from_value::<StaticModelPolicyConfig>(json!({ "model_catalog": [], "x": 1 }))
            .is_err()
    );
    assert!(serde_json::from_value::<StaticModelPolicyConfig>(json!({ "vendor": "v" })).is_err());
    let d = StaticModelPolicyConfig::default();
    assert!(d.model_catalog.is_empty());
    assert_eq!(d.vendor, "constructorfabric");
    assert_eq!(d.priority, 100);
}

#[tokio::test]
async fn snapshot_version_is_1() {
    let c = cfg(json!({
        "model_catalog": [entry(1_000_000, 3_000_000, 4)],
        "kill_switches": { "disable_images": true }
    }));
    let svc = StaticModelPolicyService::from_config(&c).unwrap();
    let user = Uuid::new_v4();
    let v = svc.get_current_policy_version(user).await.unwrap();
    assert_eq!(v.policy_version, 1);
    let snap = svc.get_policy_snapshot(user, 1).await.unwrap();
    assert_eq!(snap.policy_version, 1);
    assert_eq!(snap.model_catalog.len(), 1);
    assert_eq!(snap.model_catalog[0].id, "m1");
    assert!(snap.kill_switches.disable_images);
    let limits = svc.get_user_limits(user, 1).await.unwrap();
    assert_eq!(limits.user_id, user);
    assert_eq!(limits.policy_version, 1);
    let lic = svc.check_user_license(user).await.unwrap();
    assert!(!lic.active);
}

#[tokio::test]
async fn default_limits() {
    let c = StaticModelPolicyConfig::default();
    assert_eq!(
        c.default_standard_limits.limit_daily_credits_micro,
        100_000_000
    );
    assert_eq!(
        c.default_standard_limits.limit_monthly_credits_micro,
        1_000_000_000
    );
    assert_eq!(
        c.default_premium_limits.limit_daily_credits_micro,
        50_000_000
    );
    assert_eq!(
        c.default_premium_limits.limit_monthly_credits_micro,
        500_000_000
    );

    let svc = StaticModelPolicyService::from_config(&c).unwrap();
    let limits = svc.get_user_limits(Uuid::new_v4(), 1).await.unwrap();
    assert_eq!(limits.standard.limit_daily_credits_micro, 100_000_000);
    assert_eq!(limits.standard.limit_monthly_credits_micro, 1_000_000_000);
    assert_eq!(limits.premium.limit_daily_credits_micro, 50_000_000);
    assert_eq!(limits.premium.limit_monthly_credits_micro, 500_000_000);

    let c = cfg(json!({
        "model_catalog": [],
        "default_premium_limits": { "limit_daily_credits_micro": 7, "limit_monthly_credits_micro": 8 }
    }));
    let svc = StaticModelPolicyService::from_config(&c).unwrap();
    let limits = svc.get_user_limits(Uuid::new_v4(), 1).await.unwrap();
    assert_eq!(limits.premium.limit_daily_credits_micro, 7);
    assert_eq!(limits.standard.limit_daily_credits_micro, 100_000_000);
}
