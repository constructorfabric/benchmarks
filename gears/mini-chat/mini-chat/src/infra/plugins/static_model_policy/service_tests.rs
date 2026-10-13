#![allow(clippy::unwrap_used, clippy::expect_used)]

use mini_chat_sdk::{
    BillingOutcome, MiniChatModelPolicyPluginClientV1, ModelPolicyPluginError, RequesterType,
    SettlementMethod, UsageEvent, UsageTerminalState,
};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use super::{STATIC_POLICY_VERSION, StaticModelPolicyConfig, StaticModelPolicyService};

fn service() -> StaticModelPolicyService {
    let config: StaticModelPolicyConfig = serde_json::from_value(json!({
        "model_catalog": [],
        "kill_switches": { "force_standard_tier": true },
        "default_premium_limits": {
            "limit_daily_credits_micro": 7, "limit_monthly_credits_micro": 70
        }
    }))
    .unwrap();
    StaticModelPolicyService::from_config(config)
}

#[tokio::test]
async fn the_current_policy_version_is_the_fixed_version() {
    let version = service()
        .get_current_policy_version(Uuid::new_v4())
        .await
        .unwrap();

    assert_eq!(version.policy_version, STATIC_POLICY_VERSION);
}

#[tokio::test]
async fn the_snapshot_serves_the_configured_policy_for_version_one_only() {
    let service = service();

    let snapshot = service
        .get_policy_snapshot(Uuid::new_v4(), STATIC_POLICY_VERSION)
        .await
        .unwrap();

    assert_eq!(snapshot.policy_version, STATIC_POLICY_VERSION);
    assert!(snapshot.kill_switches.force_standard_tier);
    assert!(!snapshot.kill_switches.disable_premium_tier);
    assert!(matches!(
        service.get_policy_snapshot(Uuid::new_v4(), 2).await,
        Err(ModelPolicyPluginError::PolicyVersionNotFound { policy_version: 2 })
    ));
}

#[tokio::test]
async fn user_limits_are_the_same_configured_defaults_for_every_user() {
    let service = service();
    let user = Uuid::new_v4();

    let limits = service
        .get_user_limits(user, STATIC_POLICY_VERSION)
        .await
        .unwrap();

    assert_eq!(limits.user_id, user);
    assert_eq!(limits.policy_version, STATIC_POLICY_VERSION);
    assert_eq!(limits.standard.limit_daily_credits_micro, 100_000_000);
    assert_eq!(limits.premium.limit_daily_credits_micro, 7);
    assert_eq!(limits.premium.limit_monthly_credits_micro, 70);
    assert!(service.get_user_limits(user, 9).await.is_err());
}

#[tokio::test]
async fn publish_usage_accepts_the_event_and_the_license_check_defaults_to_inactive() {
    let service = service();
    let event = UsageEvent {
        tenant_id: Uuid::new_v4(),
        user_id: Some(Uuid::new_v4()),
        chat_id: Uuid::new_v4(),
        turn_id: Some(Uuid::new_v4()),
        request_id: Uuid::new_v4(),
        effective_model: "m".to_owned(),
        selected_model: "m".to_owned(),
        terminal_state: UsageTerminalState::Completed,
        billing_outcome: BillingOutcome::Completed,
        usage: None,
        actual_credits_micro: 1,
        settlement_method: SettlementMethod::Actual,
        policy_version_applied: STATIC_POLICY_VERSION,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        file_search_calls: 0,
        timestamp: OffsetDateTime::UNIX_EPOCH,
        requester_type: RequesterType::User,
        dedupe_key: "k".to_owned(),
        system_task_type: None,
    };

    assert!(service.publish_usage(event).await.is_ok());
    assert!(
        !service
            .check_user_license(Uuid::new_v4())
            .await
            .unwrap()
            .active
    );
}
