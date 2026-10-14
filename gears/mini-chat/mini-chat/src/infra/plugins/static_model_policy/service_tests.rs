//! The fixed-snapshot service: policy version 1, configured catalog and kill switches, the
//! same user limits for every user, `publish_usage` as a log write only.
//!
//! Slice S1, algorithm `static-policy-plugin` (steps 3, 7 and 8).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mini_chat_sdk::{
    KillSwitches, MiniChatModelPolicyPluginClientV1, ModelPolicyPluginError, TierLimits, UsageEvent,
};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use super::StaticModelPolicyService;
use crate::infra::plugins::static_model_policy::config::StaticModelPolicyConfig;
use crate::test_support::{USER_A1, USER_B1, entry_json};

fn configured() -> StaticModelPolicyConfig {
    let mut disabled = entry_json("gpt-off", "premium", false);
    disabled["description"] = json!("hidden");
    StaticModelPolicyConfig::parse(json!({
        "model_catalog": [entry_json("gpt-on", "standard", true), disabled],
        "kill_switches": { "disable_images": true, "force_standard_tier": true },
        "default_standard_limits": { "limit_daily_credits_micro": 11, "limit_monthly_credits_micro": 22 },
        "default_premium_limits": { "limit_daily_credits_micro": 33, "limit_monthly_credits_micro": 44 }
    }))
    .expect("config parses")
}

fn service() -> StaticModelPolicyService {
    StaticModelPolicyService::new(configured()).expect("valid config")
}

fn usage_event() -> UsageEvent {
    UsageEvent {
        tenant_id: Uuid::from_u128(1),
        user_id: Some(USER_A1),
        chat_id: Uuid::from_u128(3),
        turn_id: Some(Uuid::from_u128(4)),
        request_id: Uuid::from_u128(5),
        effective_model: "gpt-on".to_owned(),
        selected_model: "gpt-on".to_owned(),
        terminal_state: "completed".to_owned(),
        billing_outcome: "completed".to_owned(),
        usage: None,
        actual_credits_micro: 1,
        settlement_method: "actual".to_owned(),
        policy_version_applied: 1,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        file_search_calls: 0,
        timestamp: OffsetDateTime::UNIX_EPOCH,
        requester_type: "user".to_owned(),
        dedupe_key: None,
        system_task_type: None,
    }
}

#[tokio::test]
async fn the_policy_version_is_always_one() {
    let svc = service();
    for user in [USER_A1, USER_B1, Uuid::nil()] {
        assert_eq!(svc.get_current_policy_version(user).await.unwrap(), 1);
    }
}

#[tokio::test]
async fn the_snapshot_carries_the_configured_catalog_and_kill_switches() {
    let svc = service();
    let snapshot = svc.get_policy_snapshot(USER_A1, 1).await.unwrap();
    assert_eq!(snapshot.policy_version, 1);
    let ids: Vec<&str> = snapshot
        .model_catalog
        .iter()
        .map(|e| e.id.as_str())
        .collect();
    assert_eq!(
        ids,
        vec!["gpt-on", "gpt-off"],
        "the plugin serves every entry; visibility filters later"
    );
    assert!(snapshot.model_catalog[0].enabled);
    assert!(!snapshot.model_catalog[1].enabled);
    assert_eq!(
        snapshot.kill_switches,
        KillSwitches {
            disable_premium_tier: false,
            force_standard_tier: true,
            disable_web_search: false,
            disable_file_search: false,
            disable_images: true,
            disable_code_interpreter: false,
        }
    );
}

#[tokio::test]
async fn the_snapshot_is_identical_for_every_user_and_every_call() {
    let svc = service();
    let first = svc.get_policy_snapshot(USER_A1, 1).await.unwrap();
    assert_eq!(first, svc.get_policy_snapshot(USER_A1, 1).await.unwrap());
    assert_eq!(first, svc.get_policy_snapshot(USER_B1, 1).await.unwrap());
}

#[tokio::test]
async fn a_version_other_than_one_does_not_exist() {
    let svc = service();
    for version in [0u64, 2, 99] {
        let err = svc.get_policy_snapshot(USER_A1, version).await.unwrap_err();
        assert_eq!(err, ModelPolicyPluginError::PolicyVersionNotFound(version));
    }
}

#[tokio::test]
async fn every_user_gets_the_same_limits_standard_for_total_and_premium_for_the_premium_bucket() {
    let svc = service();
    for user in [USER_A1, USER_B1] {
        let limits = svc.get_user_limits(user, 1).await.unwrap();
        assert_eq!(limits.user_id, user);
        assert_eq!(limits.policy_version, 1);
        assert_eq!(
            limits.standard,
            TierLimits {
                limit_daily_credits_micro: 11,
                limit_monthly_credits_micro: 22
            }
        );
        assert_eq!(
            limits.premium,
            TierLimits {
                limit_daily_credits_micro: 33,
                limit_monthly_credits_micro: 44
            }
        );
    }
}

#[tokio::test]
async fn default_limits_apply_when_the_section_is_absent() {
    let svc = StaticModelPolicyService::new(StaticModelPolicyConfig::default()).unwrap();
    let limits = svc.get_user_limits(USER_A1, 1).await.unwrap();
    assert_eq!(limits.standard.limit_daily_credits_micro, 100_000_000);
    assert_eq!(limits.standard.limit_monthly_credits_micro, 1_000_000_000);
    assert_eq!(limits.premium.limit_daily_credits_micro, 50_000_000);
    assert_eq!(limits.premium.limit_monthly_credits_micro, 500_000_000);
    let snapshot = svc.get_policy_snapshot(USER_A1, 1).await.unwrap();
    assert!(snapshot.model_catalog.is_empty());
    assert_eq!(snapshot.kill_switches, KillSwitches::default());
}

#[tokio::test]
async fn publishing_usage_is_a_log_write_only_and_always_succeeds() {
    let svc = service();
    svc.publish_usage(usage_event())
        .await
        .expect("publish succeeds");
    svc.publish_usage(usage_event())
        .await
        .expect("and is repeatable");
}

#[tokio::test]
async fn the_license_check_keeps_its_default_inactive_body() {
    let status = service().check_user_license(USER_A1).await.unwrap();
    assert!(!status.active);
}

#[test]
fn building_the_service_fails_for_an_out_of_range_multiplier() {
    let mut cfg = configured();
    cfg.model_catalog[0].input_tokens_credit_multiplier_micro = 0;
    assert!(StaticModelPolicyService::new(cfg).is_err());
    let mut cfg = configured();
    cfg.model_catalog[1].output_tokens_credit_multiplier_micro = 10_000_000_001;
    assert!(StaticModelPolicyService::new(cfg).is_err());
}
