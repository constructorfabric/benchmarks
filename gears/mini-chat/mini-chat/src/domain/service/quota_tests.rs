#![allow(clippy::unwrap_used, clippy::expect_used)]

use mini_chat_sdk::{KillSwitches, ModelCatalogEntry, PolicySnapshot, TierLimits, UserLimits};
use uuid::Uuid;

use super::*;

fn entry(id: &str, tier: &str, enabled: bool, default: bool, tools: bool) -> ModelCatalogEntry {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "provider_model_id": id,
        "display_name": id,
        "provider_id": "openai",
        "tier": tier,
        "enabled": enabled,
        "context_window": 128_000,
        "max_output_tokens": 1000,
        "input_tokens_credit_multiplier_micro": 1_000_000,
        "output_tokens_credit_multiplier_micro": 2_000_000,
        "general_config": {"tool_support": {"web_search": tools, "file_search": tools, "code_interpreter": tools}},
        "preference": {"is_default": default, "sort_order": 0}
    }))
    .unwrap()
}

fn snapshot(ks: KillSwitches) -> PolicySnapshot {
    PolicySnapshot {
        policy_version: 7,
        model_catalog: vec![
            entry("prem", "premium", true, true, true),
            entry("prem-off", "premium", false, false, true),
            entry("std-a", "standard", true, false, false),
            entry("std-b", "standard", true, true, true),
        ],
        kill_switches: ks,
    }
}

fn limits(std: i64, prem: i64) -> UserLimits {
    UserLimits {
        user_id: Uuid::nil(),
        policy_version: 7,
        standard: TierLimits {
            limit_daily_credits_micro: std,
            limit_monthly_credits_micro: std * 10,
        },
        premium: TierLimits {
            limit_daily_credits_micro: prem,
            limit_monthly_credits_micro: prem * 10,
        },
    }
}

fn inputs(model: &str) -> PreflightInputs<'_> {
    PreflightInputs {
        selected_model: model,
        message_bytes: 400,
        prior_context_tokens: 0,
        image_count: 0,
        has_ready_documents: false,
        has_ready_code_files: false,
        web_search_requested: false,
    }
}

fn periods() -> Periods {
    Periods::at(time::OffsetDateTime::now_utc())
}

fn run(
    model: &str,
    ks: KillSwitches,
    usage: UsageState,
    l: &UserLimits,
) -> DomainResult<QuotaDecision> {
    resolve_effective_model(
        &inputs(model),
        &snapshot(ks),
        l,
        &usage,
        32_768,
        50,
        periods(),
    )
}

#[test]
fn allow_selected_premium() {
    let d = run(
        "prem",
        KillSwitches::default(),
        UsageState::default(),
        &limits(1_000_000_000, 1_000_000_000),
    )
    .unwrap();
    assert_eq!(d.effective.id, "prem");
    assert!(!d.is_downgrade() && d.is_premium());
    assert_eq!(d.downgrade_reason, None);
    assert_eq!(d.policy_version, 7);
    // estimated input: ceil((100 + 100) * 1.1) = 220; output cap 1000
    assert_eq!(d.reserve.reserve_tokens, 220 + 1000);
    assert_eq!(d.reserve.reserved_credits_micro, 220 + 2000);
    assert_eq!(d.minimal_generation_floor_applied, 50);
}

#[test]
fn premium_exhausted_downgrades_to_default_standard() {
    let usage = UsageState {
        premium_daily: BucketState {
            spent: 10_000,
            ..BucketState::default()
        },
        ..UsageState::default()
    };
    let d = run(
        "prem",
        KillSwitches::default(),
        usage,
        &limits(1_000_000_000, 10_000),
    )
    .unwrap();
    assert_eq!(d.effective.id, "std-b"); // is_default within the standard tier
    assert!(d.is_downgrade());
    assert_eq!(
        d.downgrade_reason.as_deref(),
        Some("premium_quota_exhausted")
    );
    assert!(!d.is_premium());
}

#[test]
fn reserve_must_fit_remaining_budget() {
    // 2000 left in the premium monthly subcap; the reserve is 2220
    let usage = UsageState {
        premium_monthly: BucketState {
            spent: 98_000,
            ..BucketState::default()
        },
        ..UsageState::default()
    };
    let d = run(
        "prem",
        KillSwitches::default(),
        usage,
        &limits(1_000_000_000, 10_000),
    )
    .unwrap();
    assert_eq!(d.effective.id, "std-b");
}

#[test]
fn reserved_by_others_counts() {
    let usage = UsageState {
        total_daily: BucketState {
            reserved: 1_000_000_000,
            ..BucketState::default()
        },
        ..UsageState::default()
    };
    let e = run(
        "std-a",
        KillSwitches::default(),
        usage,
        &limits(1_000_000_000, 1),
    )
    .unwrap_err();
    assert!(matches!(
        e,
        DomainError::QuotaExceeded {
            scope: crate::domain::error::QuotaScope::Tokens
        }
    ));
}

#[test]
fn standard_never_upgrades() {
    let usage = UsageState {
        total_monthly: BucketState {
            spent: i64::MAX >> 1,
            ..BucketState::default()
        },
        ..UsageState::default()
    };
    assert!(
        run(
            "std-a",
            KillSwitches::default(),
            usage,
            &limits(1_000_000_000, 1_000_000_000)
        )
        .is_err()
    );
    let d = run(
        "std-a",
        KillSwitches::default(),
        UsageState::default(),
        &limits(1_000_000_000, 1_000_000_000),
    )
    .unwrap();
    assert_eq!(d.effective.id, "std-a");
    assert!(!d.is_downgrade());
}

#[test]
fn kill_switches_and_disabled_models() {
    let l = limits(1_000_000_000, 1_000_000_000);
    let d = run(
        "prem",
        KillSwitches {
            force_standard_tier: true,
            ..KillSwitches::default()
        },
        UsageState::default(),
        &l,
    )
    .unwrap();
    assert_eq!(d.downgrade_reason.as_deref(), Some("force_standard_tier"));
    let d = run(
        "prem",
        KillSwitches {
            disable_premium_tier: true,
            ..KillSwitches::default()
        },
        UsageState::default(),
        &l,
    )
    .unwrap();
    assert_eq!(d.downgrade_reason.as_deref(), Some("disable_premium_tier"));
    // disabled premium model: cascade picks the premium default, reason model_disabled
    let d = run(
        "prem-off",
        KillSwitches::default(),
        UsageState::default(),
        &l,
    )
    .unwrap();
    assert_eq!(d.effective.id, "prem");
    assert_eq!(d.downgrade_reason.as_deref(), Some("model_disabled"));
    assert!(d.is_downgrade());
    // a model missing from the catalog starts at premium
    let d = run("gone", KillSwitches::default(), UsageState::default(), &l).unwrap();
    assert_eq!(d.effective.id, "prem");
    assert_eq!(d.downgrade_reason.as_deref(), Some("model_disabled"));
}

#[test]
fn tool_surcharges_follow_tool_support() {
    let l = limits(1_000_000_000, 1_000_000_000);
    let mut i = inputs("std-a");
    i.has_ready_documents = true;
    i.has_ready_code_files = true;
    i.web_search_requested = true;
    let s = snapshot(KillSwitches::default());
    let d =
        resolve_effective_model(&i, &s, &l, &UsageState::default(), 32_768, 50, periods()).unwrap();
    assert_eq!(d.tools, ToolPlan::default()); // std-a supports no tools
    assert_eq!(d.reserve.estimated_input_tokens, 220);
    let mut i = inputs("std-b");
    i.has_ready_documents = true;
    i.has_ready_code_files = true;
    i.web_search_requested = true;
    let d =
        resolve_effective_model(&i, &s, &l, &UsageState::default(), 32_768, 50, periods()).unwrap();
    assert_eq!(
        d.tools,
        ToolPlan {
            file_search: true,
            web_search: true,
            code_interpreter: true
        }
    );
    assert_eq!(d.reserve.estimated_input_tokens, 220 + 500 + 500 + 1000);
    // kill switches remove file search / code interpreter
    let s = snapshot(KillSwitches {
        disable_file_search: true,
        disable_code_interpreter: true,
        ..KillSwitches::default()
    });
    let d =
        resolve_effective_model(&i, &s, &l, &UsageState::default(), 32_768, 50, periods()).unwrap();
    assert!(!d.tools.file_search && !d.tools.code_interpreter && d.tools.web_search);
}

#[test]
fn max_output_applied_is_capped_by_config() {
    let e = entry("x", "standard", true, false, false);
    assert_eq!(max_output_applied(&e, 500), 500);
    assert_eq!(max_output_applied(&e, 32_768), 1000);
}

#[test]
fn period_starts() {
    let p = Periods::at(time::macros::datetime!(2026-02-28 23:59:59 UTC));
    assert_eq!(p.daily, time::macros::date!(2026 - 02 - 28));
    assert_eq!(p.monthly, time::macros::date!(2026 - 02 - 01));
}
