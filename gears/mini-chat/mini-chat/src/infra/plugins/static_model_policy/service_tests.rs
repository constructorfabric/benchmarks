#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Tests of the serving part of
//! `cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin`
//! (steps `inst-serve-version` .. `inst-keep-license-default`).

use mini_chat_sdk::{
    BillingOutcome, KillSwitches, MiniChatModelPolicyPluginClientV1, RequesterType,
    SettlementMethod, TerminalState, TierLimits, UsageEvent, decode_model_catalog_entry,
};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use super::{StaticModelPolicyConfig, StaticModelPolicyService};

fn user() -> Uuid {
    Uuid::from_u128(0x55)
}

fn catalog_entry(id: &str) -> mini_chat_sdk::ModelCatalogEntry {
    decode_model_catalog_entry(&json!({
        "id": id,
        "provider_model_id": id,
        "display_name": "Model",
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
        }
    }))
    .expect("valid catalog entry")
}

fn config() -> StaticModelPolicyConfig {
    StaticModelPolicyConfig {
        vendor: "constructorfabric".to_owned(),
        priority: 100,
        model_catalog: vec![catalog_entry("gpt-5.2"), catalog_entry("gpt-4.1-mini")],
        kill_switches: KillSwitches {
            disable_premium_tier: false,
            force_standard_tier: true,
            disable_web_search: true,
            disable_file_search: false,
            disable_images: false,
            disable_code_interpreter: false,
        },
        default_standard_limits: TierLimits {
            limit_daily_credits_micro: 100_000_000,
            limit_monthly_credits_micro: 1_000_000_000,
        },
        default_premium_limits: TierLimits {
            limit_daily_credits_micro: 50_000_000,
            limit_monthly_credits_micro: 500_000_000,
        },
    }
}

fn service() -> StaticModelPolicyService {
    StaticModelPolicyService::new(config())
}

fn usage_event() -> UsageEvent {
    UsageEvent {
        tenant_id: Uuid::from_u128(0x7E),
        user_id: Some(user()),
        chat_id: Uuid::from_u128(0xC1),
        turn_id: Some(Uuid::from_u128(0x71)),
        request_id: Uuid::from_u128(0x91),
        effective_model: "gpt-5.2".to_owned(),
        selected_model: "gpt-5.2".to_owned(),
        terminal_state: TerminalState::Completed,
        billing_outcome: BillingOutcome::Completed,
        usage: None,
        actual_credits_micro: 0,
        settlement_method: SettlementMethod::None,
        policy_version_applied: 1,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        file_search_calls: 0,
        timestamp: OffsetDateTime::UNIX_EPOCH,
        requester_type: RequesterType::User,
        dedupe_key: "k".to_owned(),
        system_task_type: None,
    }
}

#[tokio::test]
async fn serves_policy_version_1() {
    let info = service()
        .get_current_policy_version(user())
        .await
        .expect("version");

    assert_eq!(info.policy_version, 1);
}

#[tokio::test]
async fn serves_the_configured_snapshot_for_version_1() {
    let cfg = config();

    let snapshot = service()
        .get_policy_snapshot(user(), 1)
        .await
        .expect("snapshot");

    assert_eq!(snapshot.policy_version, 1);
    assert_eq!(snapshot.model_catalog, cfg.model_catalog);
    assert_eq!(snapshot.kill_switches, cfg.kill_switches);
}

#[tokio::test]
async fn the_snapshot_is_the_same_for_every_user() {
    let service = service();

    let first = service.get_policy_snapshot(user(), 1).await.expect("first");
    let second = service
        .get_policy_snapshot(Uuid::from_u128(0x56), 1)
        .await
        .expect("second");

    assert_eq!(first, second);
}

#[tokio::test]
async fn a_snapshot_of_any_other_version_is_a_plugin_error() {
    let service = service();
    service
        .get_policy_snapshot(user(), 1)
        .await
        .expect("control: version 1 is served");

    for version in [0_u64, 2, 12345, u64::MAX] {
        let result = service.get_policy_snapshot(user(), version).await;

        assert!(result.is_err(), "version {version} must be an error");
    }
}

#[tokio::test]
async fn serves_default_limits_for_the_requesting_user_at_version_1() {
    let cfg = config();

    let limits = service().get_user_limits(user(), 1).await.expect("limits");

    assert_eq!(limits.user_id, user());
    assert_eq!(limits.policy_version, 1);
    assert_eq!(limits.standard, cfg.default_standard_limits);
    assert_eq!(limits.premium, cfg.default_premium_limits);
}

#[tokio::test]
async fn limits_are_identical_for_every_user() {
    let service = service();
    let other = Uuid::from_u128(0x56);

    let first = service.get_user_limits(user(), 1).await.expect("first");
    let second = service.get_user_limits(other, 1).await.expect("second");

    assert_eq!(second.user_id, other);
    assert_eq!(first.standard, second.standard);
    assert_eq!(first.premium, second.premium);
}

#[tokio::test]
async fn limits_of_any_other_version_are_a_plugin_error() {
    let service = service();
    service
        .get_user_limits(user(), 1)
        .await
        .expect("control: version 1 is served");

    for version in [0_u64, 2, u64::MAX] {
        let result = service.get_user_limits(user(), version).await;

        assert!(result.is_err(), "version {version} must be an error");
    }
}

#[tokio::test]
async fn publish_usage_succeeds() {
    let result = service().publish_usage(usage_event()).await;

    assert!(result.is_ok(), "publish_usage only logs: {result:?}");
}

#[tokio::test]
async fn check_user_license_keeps_the_trait_default() {
    let status = service()
        .check_user_license(user())
        .await
        .expect("default body");

    assert!(!status.active);
}
