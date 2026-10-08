#![allow(clippy::unwrap_used, clippy::expect_used)]

use mini_chat_sdk::{
    MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginError, ModelTier, UsageEvent,
};
use serde_json::{Value, json};
use uuid::Uuid;

use super::config::StaticModelPolicyConfig;
use super::service::StaticModelPolicyService;

fn entry(id: &str, tier: &str, in_mult: i64, out_mult: i64, bytes_per_token: u32) -> Value {
    json!({
        "id": id,
        "provider_model_id": id,
        "display_name": id,
        "provider_id": "openai",
        "provider_display_name": "OpenAI",
        "tier": tier,
        "enabled": true,
        "context_window": 128_000,
        "max_output_tokens": 4096,
        "max_input_tokens": 100_000,
        "input_tokens_credit_multiplier_micro": in_mult,
        "output_tokens_credit_multiplier_micro": out_mult,
        "estimation_budgets": { "bytes_per_token_conservative": bytes_per_token },
        "max_num_results": 5,
        "general_config": {
            "type": "",
            "available_from": "1970-01-01T00:00:00Z",
            "max_file_size_mb": 25,
            "api_params": {},
            "features": {},
            "tool_support": {},
            "supported_endpoints": {}
        }
    })
}

fn parse(v: Value) -> Result<StaticModelPolicyConfig, serde_json::Error> {
    serde_json::from_value(v)
}

fn service(catalog: Vec<Value>) -> anyhow::Result<StaticModelPolicyService> {
    let cfg = parse(json!({ "model_catalog": catalog })).unwrap();
    StaticModelPolicyService::from_config(&cfg)
}

fn usage_event() -> UsageEvent {
    serde_json::from_value(json!({
        "tenant_id": Uuid::nil(),
        "chat_id": Uuid::nil(),
        "request_id": Uuid::nil(),
        "effective_model": "m",
        "selected_model": "m",
        "terminal_state": "completed",
        "billing_outcome": "completed",
        "usage": null,
        "actual_credits_micro": 0,
        "settlement_method": "actual",
        "policy_version_applied": 1,
        "web_search_calls": 0,
        "code_interpreter_calls": 0,
        "file_search_calls": 0,
        "timestamp": "2026-10-04T00:00:00Z",
        "requester_type": "user",
        "dedupe_key": "k"
    }))
    .unwrap()
}

#[test]
fn static_policy_rejects_zero_multiplier() {
    for (i, o) in [(0, 1_000_000), (1_000_000, 0)] {
        let err = service(vec![entry("m", "standard", i, o, 4)]).err().unwrap();
        assert!(err.to_string().contains("multiplier"), "{err}");
        assert!(err.to_string().contains("`m`"), "{err}");
    }
}

#[test]
fn static_policy_rejects_multiplier_over_1e10() {
    for (i, o) in [(10_000_000_001, 1), (1, 10_000_000_001)] {
        let err = service(vec![entry("m", "standard", i, o, 4)]).err().unwrap();
        assert!(err.to_string().contains("multiplier"), "{err}");
    }
    // The bound itself is accepted.
    service(vec![entry("m", "standard", 10_000_000_000, 1, 4)]).unwrap();
}

#[test]
fn static_policy_rejects_zero_bytes_per_token() {
    let err = service(vec![entry("m", "standard", 1, 1, 0)]).err().unwrap();
    assert!(err.to_string().contains("bytes_per_token_conservative"), "{err}");
}

#[test]
fn static_policy_unknown_top_level_key_rejected() {
    let err = parse(json!({ "model_catalog": [], "model_catlog": [] })).unwrap_err();
    assert!(err.to_string().contains("unknown field"), "{err}");
}

#[test]
fn static_policy_model_catalog_required_when_section_present() {
    assert!(parse(json!({ "vendor": "acme" })).is_err());
    // Absent section: defaults with an empty catalog.
    let cfg = StaticModelPolicyConfig::default();
    assert!(cfg.model_catalog.is_empty());
    assert_eq!(cfg.vendor, "constructorfabric");
    assert_eq!(cfg.priority, 100);
}

#[test]
fn static_policy_unknown_keys_in_catalog_entry_ignored() {
    let mut e = entry("m", "standard", 1, 1, 4);
    e["something_new"] = json!(true);
    service(vec![e]).unwrap();
}

#[test]
fn static_kill_switches_default_false_and_unknown_switch_rejected() {
    let cfg = parse(json!({ "model_catalog": [] })).unwrap();
    let ks = StaticModelPolicyService::from_config(&cfg).unwrap();
    let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
    let snap = rt
        .block_on(ks.get_policy_snapshot(Uuid::nil(), 1))
        .unwrap();
    let k = snap.kill_switches;
    assert!(!k.disable_premium_tier);
    assert!(!k.force_standard_tier);
    assert!(!k.disable_web_search);
    assert!(!k.disable_file_search);
    assert!(!k.disable_images);
    assert!(!k.disable_code_interpreter);

    // Partial object: missing fields are false.
    let cfg = parse(json!({
        "model_catalog": [],
        "kill_switches": { "disable_images": true }
    }))
    .unwrap();
    let svc = StaticModelPolicyService::from_config(&cfg).unwrap();
    let snap = rt.block_on(svc.get_policy_snapshot(Uuid::nil(), 1)).unwrap();
    assert!(snap.kill_switches.disable_images);
    assert!(!snap.kill_switches.disable_web_search);

    // Unknown switch is a config error.
    let err = parse(json!({
        "model_catalog": [],
        "kill_switches": { "disable_imagess": true }
    }))
    .unwrap_err();
    assert!(err.to_string().contains("unknown field"), "{err}");
}

#[tokio::test]
async fn static_policy_serves_version_1_and_catalog_in_order() {
    let svc = service(vec![
        entry("zeta", "premium", 3, 15, 4),
        entry("alpha", "Standard", 1, 3, 4),
        entry("mid", "Premium", 2, 2, 4),
    ])
    .unwrap();
    let user = Uuid::new_v4();

    let info = svc.get_current_policy_version(user).await.unwrap();
    assert_eq!(info.policy_version, 1);
    let age = time::OffsetDateTime::now_utc() - info.generated_at;
    assert!(age.whole_seconds().abs() < 60);

    let snap = svc.get_policy_snapshot(user, 1).await.unwrap();
    assert_eq!(snap.policy_version, 1);
    let ids: Vec<_> = snap.model_catalog.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["zeta", "alpha", "mid"]);
    assert_eq!(snap.model_catalog[0].tier, ModelTier::Premium);
    assert_eq!(snap.model_catalog[1].tier, ModelTier::Standard);

    // Unknown versions are not found.
    assert!(matches!(
        svc.get_policy_snapshot(user, 2).await,
        Err(MiniChatModelPolicyPluginError::NotFound(_))
    ));
    assert!(matches!(
        svc.get_user_limits(user, 0).await,
        Err(MiniChatModelPolicyPluginError::NotFound(_))
    ));

    // publish_usage just logs.
    svc.publish_usage(usage_event()).await.unwrap();
}

#[tokio::test]
async fn static_policy_default_limits() {
    let svc = service(vec![]).unwrap();
    let user = Uuid::new_v4();
    let l = svc.get_user_limits(user, 1).await.unwrap();
    assert_eq!(l.user_id, user);
    assert_eq!(l.policy_version, 1);
    assert_eq!(l.standard.limit_daily_credits_micro, 100_000_000);
    assert_eq!(l.standard.limit_monthly_credits_micro, 1_000_000_000);
    assert_eq!(l.premium.limit_daily_credits_micro, 50_000_000);
    assert_eq!(l.premium.limit_monthly_credits_micro, 500_000_000);
}

#[tokio::test]
async fn static_policy_configured_limits_apply_to_every_user() {
    let cfg = parse(json!({
        "model_catalog": [],
        "default_standard_limits": { "limit_daily_credits_micro": 7, "limit_monthly_credits_micro": 70 },
        "default_premium_limits": { "limit_daily_credits_micro": 3, "limit_monthly_credits_micro": 30 }
    }))
    .unwrap();
    let svc = StaticModelPolicyService::from_config(&cfg).unwrap();
    for _ in 0..2 {
        let l = svc.get_user_limits(Uuid::new_v4(), 1).await.unwrap();
        assert_eq!(l.standard.limit_daily_credits_micro, 7);
        assert_eq!(l.premium.limit_monthly_credits_micro, 30);
    }
}

#[test]
fn static_policy_parses_repo_dev_config() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../config/mini-chat.yaml");
    let text = std::fs::read_to_string(path).unwrap();
    let root: Value = serde_saphyr::from_str(&text).unwrap();
    let section = &root["gears"]["static-mini-chat-model-policy-plugin"]["config"];
    let cfg = parse(section.clone()).unwrap();
    assert!(!cfg.model_catalog.is_empty());
    assert_eq!(cfg.model_catalog[0].tier, ModelTier::Premium);
    StaticModelPolicyService::from_config(&cfg).unwrap();
}
