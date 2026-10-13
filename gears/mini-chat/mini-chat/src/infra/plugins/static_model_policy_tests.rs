#![allow(clippy::unwrap_used)]

use mini_chat_sdk::MiniChatModelPolicyPluginClientV1;
use serde_json::json;
use uuid::Uuid;

use super::{StaticModelPolicy, StaticModelPolicyConfig};

fn entry(mult: i64) -> serde_json::Value {
    json!({"id": "m", "provider_model_id": "m", "display_name": "M", "provider_id": "p", "tier": "Premium",
        "enabled": true, "context_window": 1000, "max_output_tokens": 100, "max_input_tokens": 0,
        "input_tokens_credit_multiplier_micro": mult, "output_tokens_credit_multiplier_micro": 1,
        "max_num_results": 5, "general_config": {}})
}

#[test]
fn config_validation() {
    assert!(StaticModelPolicyConfig::from_value(&json!({})).is_ok());
    assert!(StaticModelPolicyConfig::from_value(&json!({"vendor": "x"})).is_err());
    assert!(StaticModelPolicyConfig::from_value(&json!({"model_catalog": []})).is_ok());
    assert!(StaticModelPolicyConfig::from_value(&json!({"model_catalog": [entry(0)]})).is_err());
    assert!(StaticModelPolicyConfig::from_value(&json!({"model_catalog": [entry(10_000_000_001_i64)]})).is_err());
    assert!(StaticModelPolicyConfig::from_value(&json!({"model_catalog": [], "kill_switches": {"bogus": true}})).is_err());
    assert!(StaticModelPolicyConfig::from_value(&json!({"model_catalog": [], "unknown": 1})).is_err());
}

#[tokio::test]
async fn serves_version_one_and_defaults() {
    let cfg = StaticModelPolicyConfig::from_value(&json!({"model_catalog": [entry(5)], "kill_switches": {"disable_images": true}})).unwrap();
    let p = StaticModelPolicy::new(&cfg);
    let u = Uuid::new_v4();
    assert_eq!(p.get_current_policy_version(u).await.unwrap().policy_version, 1);
    let s = p.get_policy_snapshot(u, 1).await.unwrap();
    assert!(s.kill_switches.disable_images);
    assert!(p.get_policy_snapshot(u, 2).await.is_err());
    let l = p.get_user_limits(u, 1).await.unwrap();
    assert_eq!(l.standard.limit_daily_credits_micro, 100_000_000);
    assert_eq!(l.premium.limit_monthly_credits_micro, 500_000_000);
}
