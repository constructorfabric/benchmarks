use super::*;
use mini_chat_sdk::{EstimationBudgets, ModelGeneralConfig, ModelPreference};

fn model(id: &str, tier: ModelTier, enabled: bool) -> ModelCatalogEntry {
    serde_json::from_value::<ModelCatalogEntry>(serde_json::json!({
        "id": id, "provider_model_id": id, "display_name": id, "provider_id": "p",
        "tier": tier.as_str(), "enabled": enabled, "context_window": 100_000,
        "max_output_tokens": 1000, "max_input_tokens": 0,
        "input_tokens_credit_multiplier_micro": 1_000_000,
        "output_tokens_credit_multiplier_micro": 1_000_000
    }))
    .unwrap()
}

fn snapshot(models: Vec<ModelCatalogEntry>, ks: KillSwitches) -> PolicySnapshot {
    PolicySnapshot { policy_version: 1, model_catalog: models, kill_switches: ks }
}

fn limits(std_daily: i64, prem_daily: i64) -> UserLimits {
    UserLimits {
        user_id: Uuid::nil(),
        policy_version: 1,
        standard: TierLimits { limit_daily_credits_micro: std_daily, limit_monthly_credits_micro: std_daily * 10 },
        premium: TierLimits { limit_daily_credits_micro: prem_daily, limit_monthly_credits_micro: prem_daily * 10 },
    }
}

fn req() -> CascadeRequest {
    CascadeRequest { streaming_max_output_tokens: 32768, ..CascadeRequest::default() }
}

fn catalog() -> Vec<ModelCatalogEntry> {
    let mut std_default = model("std", ModelTier::Standard, true);
    std_default.preference = Some(ModelPreference { is_default: true, sort_order: 0 });
    vec![model("prem", ModelTier::Premium, true), model("std-other", ModelTier::Standard, true), std_default]
}

#[test]
fn allow_when_available() {
    let s = snapshot(catalog(), KillSwitches::default());
    let d = cascade(&s, &limits(100_000_000, 50_000_000), &UsageRows::default(), "prem", &req()).unwrap();
    assert_eq!(d.effective.id, "prem");
    assert_eq!(d.decision, QuotaDecision::Allow);
    assert!(d.downgrade_reason.is_none());
    // reserve: 110 input estimate + 1000 output, 1 credit per token-million -> ceil per component
    assert_eq!(d.reserve.max_output_tokens_applied, 1000);
}

#[test]
fn premium_exhausted_downgrades_to_standard_default() {
    let s = snapshot(catalog(), KillSwitches::default());
    let mut rows = UsageRows::default();
    rows.premium_daily.spent = 50_000_000;
    let d = cascade(&s, &limits(100_000_000, 50_000_000), &rows, "prem", &req()).unwrap();
    assert_eq!(d.effective.id, "std", "is_default standard candidate");
    assert_eq!(d.decision, QuotaDecision::Downgrade);
    assert_eq!(d.downgrade_reason, Some("premium_quota_exhausted"));
}

#[test]
fn monthly_exhaustion_also_blocks_tier() {
    let s = snapshot(catalog(), KillSwitches::default());
    let mut rows = UsageRows::default();
    rows.premium_monthly.reserved = 500_000_000;
    let d = cascade(&s, &limits(100_000_000, 50_000_000), &rows, "prem", &req()).unwrap();
    assert_eq!(d.decision, QuotaDecision::Downgrade);
}

#[test]
fn all_exhausted_rejects_with_tokens_scope() {
    let s = snapshot(catalog(), KillSwitches::default());
    let mut rows = UsageRows::default();
    rows.total_daily.spent = 100_000_000;
    let err = cascade(&s, &limits(100_000_000, 50_000_000), &rows, "prem", &req()).unwrap_err();
    assert!(matches!(err, DomainError::QuotaExceeded(QuotaScope::Tokens)));
    let err = cascade(&s, &limits(100_000_000, 50_000_000), &rows, "std", &req()).unwrap_err();
    assert!(matches!(err, DomainError::QuotaExceeded(QuotaScope::Tokens)));
}

#[test]
fn reserve_must_fit_not_just_remaining() {
    let s = snapshot(catalog(), KillSwitches::default());
    let mut rows = UsageRows::default();
    // only 500 micro-credits left in total, but the reserve needs ~1110
    rows.total_daily.spent = 100_000_000 - 500;
    let err = cascade(&s, &limits(100_000_000, 50_000_000), &rows, "std", &req()).unwrap_err();
    assert!(matches!(err, DomainError::QuotaExceeded(_)));
}

#[test]
fn standard_never_upgrades() {
    let s = snapshot(catalog(), KillSwitches::default());
    let d = cascade(&s, &limits(100_000_000, 50_000_000), &UsageRows::default(), "std-other", &req()).unwrap();
    assert_eq!(d.effective.id, "std-other");
    assert_eq!(d.decision, QuotaDecision::Allow);
}

#[test]
fn disabled_or_missing_model_is_model_disabled_downgrade() {
    let mut models = catalog();
    models[0].enabled = false;
    let s = snapshot(models, KillSwitches::default());
    let d = cascade(&s, &limits(100_000_000, 50_000_000), &UsageRows::default(), "prem", &req()).unwrap();
    assert_eq!(d.effective.id, "std");
    assert_eq!(d.downgrade_reason, Some("model_disabled"));
    let d = cascade(&s, &limits(100_000_000, 50_000_000), &UsageRows::default(), "gone", &req()).unwrap();
    assert_eq!(d.downgrade_reason, Some("model_disabled"));
}

#[test]
fn kill_switches_skip_premium() {
    for (ks, reason) in [
        (KillSwitches { force_standard_tier: true, ..KillSwitches::default() }, "force_standard_tier"),
        (KillSwitches { disable_premium_tier: true, ..KillSwitches::default() }, "disable_premium_tier"),
    ] {
        let s = snapshot(catalog(), ks);
        let d = cascade(&s, &limits(100_000_000, 50_000_000), &UsageRows::default(), "prem", &req()).unwrap();
        assert_eq!(d.effective.tier, ModelTier::Standard);
        assert_eq!(d.downgrade_reason, Some(reason));
    }
}

#[test]
fn surcharges_follow_tool_support_per_candidate() {
    let mut models = catalog();
    models[0].general_config = ModelGeneralConfig::default();
    models[0].general_config.tool_support.web_search = true;
    models[0].general_config.tool_support.file_search = true;
    let s = snapshot(models.clone(), KillSwitches::default());
    let r = CascadeRequest {
        eligible_tools: ToolFlags { file_search: true, web_search: true, code_interpreter: true },
        ..req()
    };
    let d = cascade(&s, &limits(100_000_000, 50_000_000), &UsageRows::default(), "prem", &r).unwrap();
    assert!(d.tools.web_search && d.tools.file_search && !d.tools.code_interpreter);
    let b = EstimationBudgets::default();
    assert_eq!(
        d.reserve.estimated_input_tokens,
        110 + i64::from(b.tool_surcharge_tokens) + i64::from(b.web_search_surcharge_tokens)
    );
    let ks = KillSwitches { disable_file_search: true, ..KillSwitches::default() };
    let s = snapshot(models, ks);
    let d = cascade(&s, &limits(100_000_000, 50_000_000), &UsageRows::default(), "prem", &r).unwrap();
    assert!(!d.tools.file_search);
}

#[test]
fn tool_quotas_only_for_sent_tools() {
    let mut rows = UsageRows::default();
    rows.total_daily.web_search_calls = 75;
    rows.total_daily.code_interpreter_calls = 50;
    assert!(check_tool_quotas(&rows, ToolFlags::default(), 75, 50).is_ok());
    let ws = ToolFlags { web_search: true, ..ToolFlags::default() };
    assert!(matches!(check_tool_quotas(&rows, ws, 75, 50), Err(DomainError::QuotaExceeded(QuotaScope::WebSearch))));
    let ci = ToolFlags { code_interpreter: true, ..ToolFlags::default() };
    assert!(matches!(
        check_tool_quotas(&rows, ci, 75, 50),
        Err(DomainError::QuotaExceeded(QuotaScope::CodeInterpreter))
    ));
}

#[test]
fn status_flags_and_resets() {
    let starts = PeriodStarts::of(time::macros::datetime!(2026-02-28 15:30 UTC));
    assert_eq!(starts.daily, time::macros::date!(2026-02-28));
    assert_eq!(starts.monthly, time::macros::date!(2026-02-01));
    assert_eq!(next_reset(Period::Daily, starts), time::macros::datetime!(2026-03-01 0:00 UTC));
    assert_eq!(next_reset(Period::Monthly, starts), time::macros::datetime!(2026-03-01 0:00 UTC));
    let dec = PeriodStarts::of(time::macros::datetime!(2026-12-31 23:59:59 UTC));
    assert_eq!(next_reset(Period::Monthly, dec), time::macros::datetime!(2027-01-01 0:00 UTC));

    let mut rows = UsageRows::default();
    rows.premium_daily.spent = 40_000_000; // 20% left of 50M -> warning at 80% threshold
    rows.total_daily.spent = 99_500_000; // 0.5% left -> exhausted (floor 0)
    let mut l = limits(100_000_000, 50_000_000);
    l.premium.limit_monthly_credits_micro = 0; // skipped
    let st = status_entries(&rows, &l, starts, 80);
    assert_eq!(st.len(), 3);
    assert_eq!((st[0].tier, st[0].period), ("premium", Period::Daily));
    assert_eq!(st[0].remaining_percentage, 20);
    assert!(st[0].warning && !st[0].exhausted);
    let total_daily = st.iter().find(|s| s.tier == "total" && s.period == Period::Daily).unwrap();
    assert_eq!(total_daily.remaining_percentage, 0);
    assert!(total_daily.exhausted && total_daily.warning);
    assert_eq!(total_daily.used, 99_500_000);
}
