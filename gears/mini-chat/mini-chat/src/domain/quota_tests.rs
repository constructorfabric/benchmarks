use mini_chat_sdk::{TierLimits, UserLimits};

use super::*;

fn model(id: &str, tier: &str, mult: u64, enabled: bool) -> ModelCatalogEntry {
    serde_json::from_value(serde_json::json!({
        "id": id, "provider_model_id": id, "display_name": id, "provider_id": "p", "tier": tier,
        "enabled": enabled, "context_window": 128_000, "max_output_tokens": 500, "max_input_tokens": 0,
        "input_tokens_credit_multiplier_micro": mult, "output_tokens_credit_multiplier_micro": mult,
        "estimation_budgets": {"bytes_per_token_conservative": 4, "fixed_overhead_tokens": 0,
            "safety_margin_pct": 0, "image_token_budget": 1000, "tool_surcharge_tokens": 0,
            "web_search_surcharge_tokens": 0, "code_interpreter_surcharge_tokens": 0},
        "general_config": {"tool_support": {"web_search": true}}
    }))
    .unwrap()
}

fn limits(std_day: i64, std_month: i64, prem_day: i64, prem_month: i64) -> UserLimits {
    UserLimits {
        user_id: Uuid::nil(),
        policy_version: 1,
        standard: TierLimits { limit_daily_credits_micro: std_day, limit_monthly_credits_micro: std_month },
        premium: TierLimits { limit_daily_credits_micro: prem_day, limit_monthly_credits_micro: prem_month },
    }
}

#[test]
fn credits_per_component_ceil() {
    assert_eq!(credits_micro(1000, 500, 2_500_000_000, 2_500_000_000).unwrap(), 3_750_000);
    assert_eq!(credits_micro(1, 1, 1, 1).unwrap(), 2);
    assert_eq!(credits_micro(0, 0, 1, 1).unwrap(), 0);
    assert_eq!(credits_micro(1000, 200, 1_000_000, 3_000_000).unwrap(), 1600);
    assert_eq!(credits_micro(1, 1, 0, 1), Err(CreditError::ZeroMultiplier));
    assert_eq!(credits_micro(1, 1, MAX_MULT + 1, 1), Err(CreditError::MultiplierTooLarge));
    assert_eq!(credits_micro(MAX_TOKENS + 1, 0, 1, 1), Err(CreditError::InvalidTokenCount));
}

#[test]
fn text_estimate_formula() {
    let b = EstimationBudgets::default(); // 4 bpt, 100 overhead, 10%
    // ceil((ceil(10/4)=3 + 100) * 110 / 100) = ceil(113.3) = 114
    assert_eq!(estimate_text_tokens(10, &b), 114);
    assert_eq!(estimate_text_tokens(0, &b), 110);
}

#[test]
fn design_calculation_example_downgrades_to_standard() {
    // DESIGN §5.10: premium P (2.5 credits/1K), standard S (1.0 credits/1K).
    let snap = PolicySnapshot {
        policy_version: 1,
        model_catalog: vec![model("P", "premium", 2_500_000_000, true), model("S", "standard", 1_000_000_000, true)],
        kill_switches: KillSwitches::default(),
    };
    let mut usage = UsageView::default();
    usage.rows.insert(("tier:premium".into(), "daily".into()), RowValues { spent: 20_000_000, ..Default::default() });
    usage.rows.insert(("tier:premium".into(), "monthly".into()), RowValues { spent: 200_000_000, ..Default::default() });
    usage.rows.insert(("total".into(), "daily".into()), RowValues { spent: 25_000_000, ..Default::default() });
    usage.rows.insert(("total".into(), "monthly".into()), RowValues { spent: 240_000_000, ..Default::default() });
    let lim = limits(60_000_000, 600_000_000, 22_000_000, 300_000_000);
    // 4000 bytes / 4 = 1000 tokens, no overhead/margin.
    let input = PreflightInput { content_bytes: 4000, max_output_cap: 500, minimal_generation_floor: 50, ..Default::default() };
    let d = resolve_effective_model(&snap, "P", &usage, &lim, &input).unwrap();
    assert_eq!(d.effective.id, "S");
    assert_eq!(d.downgrade_reason, Some("premium_quota_exhausted"));
    assert_eq!(d.reserve.reserved_credits_micro, 1_500_000);
    assert_eq!(d.reserve.reserve_tokens, 1500);

    // Settlement by actual 900/300 -> 1.2M credits.
    let s = compute_settlement(
        SettlementMethod::Actual,
        TurnReserve { reserve_tokens: 1500, max_output_tokens_applied: 500, reserved_credits_micro: 1_500_000, minimal_generation_floor_applied: 50 },
        Some((900, 300)),
        (1_000_000_000, 1_000_000_000),
        1.1,
        (0, 0),
    )
    .unwrap();
    assert_eq!(s.committed_credits_micro, 1_200_000);
}

#[test]
fn all_tiers_exhausted_rejects() {
    let snap = PolicySnapshot {
        policy_version: 1,
        model_catalog: vec![model("P", "premium", 1_000_000, true), model("S", "standard", 1_000_000, true)],
        kill_switches: KillSwitches::default(),
    };
    let mut usage = UsageView::default();
    usage.rows.insert(("total".into(), "daily".into()), RowValues { spent: 100, ..Default::default() });
    let lim = limits(100, 1000, 50, 500);
    let input = PreflightInput { content_bytes: 4, max_output_cap: 500, minimal_generation_floor: 50, ..Default::default() };
    assert!(matches!(
        resolve_effective_model(&snap, "P", &usage, &lim, &input),
        Err(DomainError::QuotaExceeded("tokens"))
    ));
}

#[test]
fn kill_switches_and_disabled_models() {
    let mut snap = PolicySnapshot {
        policy_version: 1,
        model_catalog: vec![model("P", "premium", 1_000_000, true), model("S", "standard", 1_000_000, true)],
        kill_switches: KillSwitches { force_standard_tier: true, ..Default::default() },
    };
    let lim = limits(1_000_000_000, 1_000_000_000, 1_000_000_000, 1_000_000_000);
    let input = PreflightInput { content_bytes: 4, max_output_cap: 500, minimal_generation_floor: 50, ..Default::default() };
    let d = resolve_effective_model(&snap, "P", &UsageView::default(), &lim, &input).unwrap();
    assert_eq!((d.effective.id.as_str(), d.downgrade_reason), ("S", Some("force_standard_tier")));
    snap.kill_switches = KillSwitches { disable_premium_tier: true, ..Default::default() };
    let d = resolve_effective_model(&snap, "P", &UsageView::default(), &lim, &input).unwrap();
    assert_eq!(d.downgrade_reason, Some("disable_premium_tier"));
    snap.kill_switches = KillSwitches::default();
    snap.model_catalog[0].enabled = false;
    let d = resolve_effective_model(&snap, "P", &UsageView::default(), &lim, &input).unwrap();
    assert_eq!((d.effective.id.as_str(), d.downgrade_reason), ("S", Some("model_disabled")));
    let d = resolve_effective_model(&snap, "gone", &UsageView::default(), &lim, &input).unwrap();
    assert_eq!(d.downgrade_reason, Some("model_disabled"));
    // Standard never upgrades.
    let d = resolve_effective_model(&snap, "S", &UsageView::default(), &lim, &input).unwrap();
    assert!(!d.is_downgrade("S"));
}

#[test]
fn estimated_settlement_and_overshoot_cap() {
    let r = TurnReserve { reserve_tokens: 10_000, max_output_tokens_applied: 1000, reserved_credits_micro: 2_500_000, minimal_generation_floor_applied: 50 };
    let est = compute_settlement(SettlementMethod::Estimated, r, None, (1_000_000, 1_000_000), 1.1, (1, 0)).unwrap();
    assert_eq!(est.committed_credits_micro, credits_micro(9000, 50, 1_000_000, 1_000_000).unwrap());
    let capped = compute_settlement(SettlementMethod::Actual, r, Some((11_000, 500)), (1_000_000_000, 1_000_000_000), 1.1, (0, 0)).unwrap();
    assert_eq!(capped.committed_credits_micro, 2_500_000);
    assert!(capped.overshoot);
    let within = compute_settlement(SettlementMethod::Actual, r, Some((10_500, 0)), (1_000_000, 1_000_000), 1.1, (0, 0)).unwrap();
    assert_eq!(within.committed_credits_micro, 10_500);
}

#[test]
fn tool_quotas_only_when_tool_sent() {
    let mut usage = UsageView::default();
    usage.rows.insert(("total".into(), "daily".into()), RowValues { web_search_calls: 75, code_interpreter_calls: 50, ..Default::default() });
    assert!(check_tool_quotas(ToolSelection::default(), &usage, 75, 50).is_ok());
    assert!(matches!(
        check_tool_quotas(ToolSelection { web_search: true, ..Default::default() }, &usage, 75, 50),
        Err(DomainError::QuotaExceeded("web_search"))
    ));
    assert!(matches!(
        check_tool_quotas(ToolSelection { code_interpreter: true, ..Default::default() }, &usage, 75, 50),
        Err(DomainError::QuotaExceeded("code_interpreter"))
    ));
}

#[test]
fn status_flags_and_skips() {
    let mut usage = UsageView::default();
    usage.rows.insert(("total".into(), "daily".into()), RowValues { spent: 60, reserved: 0, ..Default::default() });
    let lim = limits(100, 1000, 0, 500);
    let now = OffsetDateTime::now_utc();
    let s = status(&usage, &lim, 80, now);
    assert!(!s.iter().any(|p| p.tier == "premium" && p.period == "daily"));
    let d = s.iter().find(|p| p.tier == "total" && p.period == "daily").unwrap();
    assert_eq!((d.remaining_percentage, d.warning, d.exhausted), (40, false, false));
    usage.rows.insert(("total".into(), "daily".into()), RowValues { spent: 99, reserved: 1, ..Default::default() });
    let d = status(&usage, &lim, 80, now).into_iter().find(|p| p.tier == "total" && p.period == "daily").unwrap();
    assert!(d.warning && d.exhausted);
}

fn dt(y: i32, m: u8, d: u8, h: u8, mi: u8, sec: u8) -> OffsetDateTime {
    Date::from_calendar_date(y, Month::try_from(m).unwrap(), d)
        .unwrap()
        .with_hms(h, mi, sec)
        .unwrap()
        .assume_utc()
}

#[test]
fn periods_and_resets() {
    let t = dt(2026, 2, 28, 23, 59, 59);
    let p = PeriodStarts::at(t);
    assert_eq!(p.daily, Date::from_calendar_date(2026, Month::February, 28).unwrap());
    assert_eq!(p.monthly, Date::from_calendar_date(2026, Month::February, 1).unwrap());
    assert_eq!(next_reset("daily", t), dt(2026, 3, 1, 0, 0, 0));
    assert_eq!(next_reset("monthly", dt(2026, 12, 15, 10, 0, 0)), dt(2027, 1, 1, 0, 0, 0));
}
