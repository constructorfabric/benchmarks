use chrono::Utc;
use mini_chat_sdk::{KillSwitches, ModelCatalogEntry, ModelTier, PolicySnapshot, TierLimits, UsageTokens, UserLimits};
use uuid::Uuid;

use super::*;

fn model(id: &str, tier: ModelTier, default: bool) -> ModelCatalogEntry {
    serde_json::from_value(serde_json::json!({
        "id": id, "provider_model_id": id, "provider_id": "p", "tier": tier, "enabled": true,
        "context_window": 100000, "max_output_tokens": 1000, "max_input_tokens": 0,
        "input_tokens_credit_multiplier_micro": 1_000_000, "output_tokens_credit_multiplier_micro": 1_000_000,
        "general_config": {"tool_support": {"web_search": true, "file_search": true, "code_interpreter": true}},
        "preference": {"is_default": default}
    }))
    .unwrap()
}

fn snapshot(ks: KillSwitches) -> PolicySnapshot {
    PolicySnapshot {
        policy_version: 1,
        model_catalog: vec![model("prem", ModelTier::Premium, true), model("std", ModelTier::Standard, false)],
        kill_switches: ks,
    }
}

fn limits(total: i64, premium: i64) -> UserLimits {
    UserLimits {
        user_id: Uuid::nil(),
        policy_version: 1,
        standard: TierLimits { limit_daily_credits_micro: total, limit_monthly_credits_micro: total * 10 },
        premium: TierLimits { limit_daily_credits_micro: premium, limit_monthly_credits_micro: premium * 10 },
    }
}

fn req(model: &str) -> PreflightRequest {
    PreflightRequest {
        selected_model: model.into(),
        message_bytes: 40,
        image_count: 0,
        prior_context_tokens: 0,
        has_ready_documents: false,
        has_ready_code_files: false,
        web_search_requested: false,
    }
}

fn run(snap: &PolicySnapshot, lim: &UserLimits, usage: &UsageSnapshot, r: &PreflightRequest) -> Result<PreflightDecision, DomainError> {
    evaluate_cascade(snap, lim, usage, r, 32768, 50, 75, 50, current_periods(Utc::now()))
}

#[test]
fn allow_on_selected_model() {
    let d = run(&snapshot(KillSwitches::default()), &limits(1_000_000, 1_000_000), &UsageSnapshot::default(), &req("prem")).unwrap();
    assert_eq!(d.effective.id, "prem");
    assert_eq!(d.quota_decision(), "allow");
    // 40 bytes: ceil((10 + 100) * 1.1) = 121 input tokens + 1000 output
    assert_eq!(d.estimated_input_tokens, 121);
    assert_eq!(d.reserve_tokens, 1121);
    assert_eq!(d.reserved_credits_micro, 1121);
    assert_eq!(d.minimal_generation_floor_applied, 50);
}

#[test]
fn premium_exhausted_downgrades() {
    let mut usage = UsageSnapshot::default();
    usage.rows.push((PeriodType::Daily, BUCKET_PREMIUM, BucketState { spent: 1_000, ..Default::default() }));
    let d = run(&snapshot(KillSwitches::default()), &limits(1_000_000, 1_000), &usage, &req("prem")).unwrap();
    assert_eq!(d.effective.id, "std");
    assert_eq!(d.quota_decision(), "downgrade");
    assert_eq!(d.downgrade_reason, Some("premium_quota_exhausted"));
}

#[test]
fn all_exhausted_rejects_with_tokens_scope() {
    let mut usage = UsageSnapshot::default();
    usage.rows.push((PeriodType::Daily, BUCKET_TOTAL, BucketState { spent: 1_000, ..Default::default() }));
    let e = run(&snapshot(KillSwitches::default()), &limits(1_000, 1_000), &usage, &req("prem")).unwrap_err();
    assert!(matches!(e, DomainError::QuotaExceeded("tokens")));
}

#[test]
fn standard_never_upgrades() {
    let mut usage = UsageSnapshot::default();
    usage.rows.push((PeriodType::Monthly, BUCKET_TOTAL, BucketState { spent: 100_000, ..Default::default() }));
    let e = run(&snapshot(KillSwitches::default()), &limits(10_000, 100_000), &usage, &req("std")).unwrap_err();
    assert!(matches!(e, DomainError::QuotaExceeded("tokens")));
}

#[test]
fn kill_switches_and_disabled_model() {
    let ks = KillSwitches { force_standard_tier: true, ..Default::default() };
    let d = run(&snapshot(ks), &limits(1_000_000, 1_000_000), &UsageSnapshot::default(), &req("prem")).unwrap();
    assert_eq!((d.effective.id.as_str(), d.downgrade_reason), ("std", Some("force_standard_tier")));
    let ks = KillSwitches { disable_premium_tier: true, ..Default::default() };
    let d = run(&snapshot(ks), &limits(1_000_000, 1_000_000), &UsageSnapshot::default(), &req("prem")).unwrap();
    assert_eq!(d.downgrade_reason, Some("disable_premium_tier"));
    let d = run(&snapshot(KillSwitches::default()), &limits(1_000_000, 1_000_000), &UsageSnapshot::default(), &req("gone")).unwrap();
    assert_eq!(d.downgrade_reason, Some("model_disabled"));
}

#[test]
fn daily_tool_quotas_apply_only_when_tool_is_sent() {
    let mut usage = UsageSnapshot::default();
    usage.rows.push((PeriodType::Daily, BUCKET_TOTAL, BucketState { web_search_calls: 75, code_interpreter_calls: 50, ..Default::default() }));
    let mut r = req("std");
    assert!(run(&snapshot(KillSwitches::default()), &limits(1_000_000, 1_000_000), &usage, &r).is_ok());
    r.web_search_requested = true;
    let e = run(&snapshot(KillSwitches::default()), &limits(1_000_000, 1_000_000), &usage, &r).unwrap_err();
    assert!(matches!(e, DomainError::QuotaExceeded("web_search")));
    let mut r = req("std");
    r.has_ready_code_files = true;
    let e = run(&snapshot(KillSwitches::default()), &limits(1_000_000, 1_000_000), &usage, &r).unwrap_err();
    assert!(matches!(e, DomainError::QuotaExceeded("code_interpreter")));
}

#[test]
fn surcharges_follow_tool_support() {
    let mut r = req("std");
    r.has_ready_documents = true;
    r.web_search_requested = true;
    let d = run(&snapshot(KillSwitches::default()), &limits(1_000_000, 1_000_000), &UsageSnapshot::default(), &r).unwrap();
    assert!(d.tools.file_search && d.tools.web_search);
    assert_eq!(d.estimated_input_tokens, 121 + 500 + 500);
    let ks = KillSwitches { disable_file_search: true, ..Default::default() };
    let d = run(&snapshot(ks), &limits(1_000_000, 1_000_000), &UsageSnapshot::default(), &r).unwrap();
    assert!(!d.tools.file_search);
}

#[test]
fn billing_derivation_table() {
    let u = UsageTokens { input_tokens: 5, ..Default::default() };
    assert_eq!(derive_billing("completed", None, None), ("completed", SettlementMethod::Actual));
    assert_eq!(derive_billing("cancelled", None, None), ("aborted", SettlementMethod::Estimated));
    assert_eq!(derive_billing("failed", Some("orphan_timeout"), None), ("aborted", SettlementMethod::Estimated));
    assert_eq!(derive_billing("failed", Some("provider_error"), Some(&u)), ("failed", SettlementMethod::Actual));
    assert_eq!(derive_billing("failed", Some("provider_error"), Some(&UsageTokens::default())), ("failed", SettlementMethod::Estimated));
    assert_eq!(derive_billing("failed", Some("web_search_calls_exceeded"), None), ("failed", SettlementMethod::Estimated));
    assert_eq!(derive_billing("failed", Some("turn_setup_failed"), None), ("failed", SettlementMethod::Released));
    assert_eq!(derive_billing("failed", Some("weird"), Some(&u)), ("failed", SettlementMethod::Estimated));
}

#[test]
fn settlement_amounts() {
    let r = TurnReserve { reserve_tokens: 10_000, max_output_tokens_applied: 1_000, reserved_credits_micro: 2_500_000, floor_applied: 50, in_mult: 250_000_000, out_mult: 250_000_000 };
    let u = UsageTokens { input_tokens: 11_000, output_tokens: 500, ..Default::default() };
    let s = compute_settlement(SettlementMethod::Actual, Some(&u), &r, 1.10).unwrap();
    assert_eq!(s.committed_credits, 2_500_000); // capped at reserve (1.15 > 1.10)
    assert!(s.overshoot);
    let u = UsageTokens { input_tokens: 100, output_tokens: 50, ..Default::default() };
    let s = compute_settlement(SettlementMethod::Actual, Some(&u), &r, 1.10).unwrap();
    assert_eq!(s.committed_credits, 37_500);
    let s = compute_settlement(SettlementMethod::Estimated, None, &r, 1.10).unwrap();
    assert_eq!(s.committed_credits, 9_000 * 250 + 50 * 250);
    let s = compute_settlement(SettlementMethod::Released, None, &r, 1.10).unwrap();
    assert_eq!(s.committed_credits, 0);
}

#[test]
fn status_skips_zero_limits_and_flags() {
    let mut usage = UsageSnapshot::default();
    usage.rows.push((PeriodType::Daily, BUCKET_TOTAL, BucketState { spent: 900, reserved: 50, ..Default::default() }));
    let mut lim = limits(1_000, 0);
    lim.premium.limit_monthly_credits_micro = 0;
    let s = quota_status(&lim, &usage, Utc::now(), 80);
    assert!(s.iter().all(|e| e.tier == "total"));
    let daily = s.iter().find(|e| e.period == PeriodType::Daily).unwrap();
    assert_eq!((daily.used, daily.remaining, daily.remaining_percentage), (950, 50, 5));
    assert!(daily.warning && !daily.exhausted);
}
