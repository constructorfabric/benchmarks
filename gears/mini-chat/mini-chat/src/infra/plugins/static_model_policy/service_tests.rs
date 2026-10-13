#![allow(clippy::unwrap_used, clippy::expect_used)]

use mini_chat_sdk::{MiniChatModelPolicyPluginClientV1, ModelCatalogEntry, ModelTier, TierLimits};
use serde_json::json;
use uuid::Uuid;

use super::config::StaticModelPolicyPluginConfig;
use super::service::StaticModelPolicyService;

fn entry_json(id: &str, input_mult: i64, output_mult: i64) -> serde_json::Value {
    json!({
        "id": id,
        "provider_model_id": id,
        "display_name": id,
        "provider_id": "p",
        "provider_display_name": "P",
        "tier": "standard",
        "context_window": 1000,
        "max_output_tokens": 100,
        "max_input_tokens": 900,
        "input_tokens_credit_multiplier_micro": input_mult,
        "output_tokens_credit_multiplier_micro": output_mult,
        "max_num_results": 5,
        "general_config": {
            "type": "",
            "available_from": "1970-01-01T00:00:00Z",
            "max_file_size_mb": 25,
            "api_params": {"stop": []},
            "features": {"streaming": true, "structured_output": false},
            "tool_support": {
                "web_search": false, "file_search": false, "image_generation": false,
                "code_interpreter": false, "mcp": false
            },
            "supported_endpoints": {
                "chat_completions": true, "responses": true, "embeddings": false,
                "image_generation": false, "audio_speech_generation": false,
                "audio_transcription": false, "audio_translation": false
            }
        },
        "enabled": true
    })
}

fn config_from(
    value: serde_json::Value,
) -> Result<StaticModelPolicyPluginConfig, serde_json::Error> {
    serde_json::from_value(value)
}

#[tokio::test]
async fn snapshot_from_config_catalog() {
    let cfg = config_from(json!({
        "model_catalog": [entry_json("b", 1_000_000, 2_000_000), entry_json("a", 3, 4)]
    }))
    .unwrap();
    cfg.validate().unwrap();
    let svc = StaticModelPolicyService::from_config(&cfg);
    let user = Uuid::new_v4();

    let snap = svc.get_policy_snapshot(user, 1).await.unwrap();
    assert_eq!(snap.policy_version, 1);
    let ids: Vec<_> = snap.model_catalog.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["b", "a"], "catalog keeps config order");
    assert_eq!(snap.model_catalog[0].tier, ModelTier::Standard);
    let ks = snap.kill_switches;
    assert!(!ks.disable_premium_tier && !ks.force_standard_tier);
    assert!(!ks.disable_web_search && !ks.disable_file_search);
    assert!(!ks.disable_images && !ks.disable_code_interpreter);

    let version = svc.get_current_policy_version(user).await.unwrap();
    assert_eq!(version.policy_version, 1);
}

#[tokio::test]
async fn kill_switches_are_mirrored_from_config() {
    let cfg = config_from(json!({
        "model_catalog": [],
        "kill_switches": {"disable_web_search": true, "force_standard_tier": true}
    }))
    .unwrap();
    let svc = StaticModelPolicyService::from_config(&cfg);
    let snap = svc.get_policy_snapshot(Uuid::new_v4(), 1).await.unwrap();
    assert!(snap.kill_switches.disable_web_search);
    assert!(snap.kill_switches.force_standard_tier);
    assert!(!snap.kill_switches.disable_premium_tier);
}

#[test]
fn rejects_zero_multiplier_entry() {
    for (input, output) in [(0, 1), (1, 0), (-5, 1), (1, 10_000_000_001)] {
        let cfg = config_from(json!({"model_catalog": [entry_json("m", input, output)]})).unwrap();
        let err = cfg.validate().expect_err("multiplier out of range");
        assert!(err.contains('m'), "error names the entry: {err}");
    }
    let ok =
        config_from(json!({"model_catalog": [entry_json("m", 1, 10_000_000_000_i64)]})).unwrap();
    ok.validate().unwrap();
}

#[test]
fn rejects_zero_bytes_per_token() {
    let mut e = entry_json("m", 1, 1);
    e["estimation_budgets"] = json!({"bytes_per_token_conservative": 0});
    let cfg = config_from(json!({"model_catalog": [e]})).unwrap();
    assert!(cfg.validate().is_err());
}

#[tokio::test]
async fn default_limits() {
    let cfg = StaticModelPolicyPluginConfig::default();
    assert_eq!(cfg.vendor, "constructorfabric");
    assert_eq!(cfg.priority, 100);
    assert!(cfg.model_catalog.is_empty());
    let svc = StaticModelPolicyService::from_config(&cfg);
    let user = Uuid::new_v4();
    let limits = svc.get_user_limits(user, 1).await.unwrap();
    assert_eq!(limits.user_id, user);
    assert_eq!(limits.policy_version, 1);
    assert_eq!(
        limits.standard,
        TierLimits {
            limit_daily_credits_micro: 100_000_000,
            limit_monthly_credits_micro: 1_000_000_000
        }
    );
    assert_eq!(
        limits.premium,
        TierLimits {
            limit_daily_credits_micro: 50_000_000,
            limit_monthly_credits_micro: 500_000_000
        }
    );
}

#[test]
fn config_rejects_unknown_kill_switch() {
    let err = config_from(json!({
        "model_catalog": [],
        "kill_switches": {"disable_premium": true}
    }))
    .expect_err("unknown kill switch");
    assert!(err.to_string().contains("disable_premium"), "{err}");
}

#[test]
fn config_rejects_unknown_top_level_key() {
    assert!(config_from(json!({"model_catalog": [], "bogus": 1})).is_err());
}

#[test]
fn config_requires_model_catalog_when_section_present() {
    let err = config_from(json!({"vendor": "x"})).expect_err("model_catalog is required");
    assert!(err.to_string().contains("model_catalog"), "{err}");
}

#[test]
fn nested_sdk_types_ignore_unknown_keys() {
    let mut e = entry_json("m", 1, 1);
    e["future_field"] = json!(true);
    let cfg = config_from(json!({
        "model_catalog": [e],
        "default_standard_limits": {
            "limit_daily_credits_micro": 1, "limit_monthly_credits_micro": 2, "extra": 3
        }
    }))
    .unwrap();
    let _: &ModelCatalogEntry = &cfg.model_catalog[0];
    assert_eq!(cfg.default_standard_limits.limit_monthly_credits_micro, 2);
}

#[test]
fn shipped_config_section_is_valid() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../config/mini-chat.yaml"
    );
    let raw = std::fs::read_to_string(path).expect("read config/mini-chat.yaml");
    let doc: serde_json::Value = serde_saphyr::from_str(&raw).expect("parse yaml");
    let section = doc["gears"]["static-mini-chat-model-policy-plugin"]["config"].clone();
    let cfg = config_from(section).expect("shipped plugin config deserializes");
    assert!(!cfg.model_catalog.is_empty());
    cfg.validate().expect("shipped plugin config validates");
}
