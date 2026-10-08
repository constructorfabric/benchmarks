use super::*;
use mini_chat_sdk::{KillSwitches, ModelPreference, TierLimits};

fn model(id: &str, tier: ModelTier, mult: i64, default: bool) -> ModelCatalogEntry {
    let mut m: ModelCatalogEntry = serde_json::from_value(serde_json::json!({
        "id": id, "provider_model_id": id, "display_name": id, "provider_id": "p",
        "tier": tier.as_str(), "enabled": true, "context_window": 128_000,
        "max_output_tokens": 500, "max_input_tokens": 0,
        "input_tokens_credit_multiplier_micro": mult, "output_tokens_credit_multiplier_micro": mult,
        "estimation_budgets": {"bytes_per_token_conservative": 1, "fixed_overhead_tokens": 0, "safety_margin_pct": 0,
            "image_token_budget": 100, "tool_surcharge_tokens": 0, "web_search_surcharge_tokens": 0,
            "code_interpreter_surcharge_tokens": 0, "minimal_generation_floor": 50}
    }))
    .expect("model");
    m.general_config.tool_support.web_search = true;
    m.preference = Some(ModelPreference {
        is_default: default,
        sort_order: 0,
    });
    m
}

fn snapshot() -> PolicySnapshot {
    PolicySnapshot {
        policy_version: 1,
        model_catalog: vec![
            model("P", ModelTier::Premium, 2_500_000_000, true),
            model("S", ModelTier::Standard, 1_000_000_000, false),
        ],
        kill_switches: KillSwitches::default(),
    }
}

fn limits() -> UserLimits {
    UserLimits {
        user_id: Uuid::nil(),
        policy_version: 1,
        standard: TierLimits {
            limit_daily_credits_micro: 60_000_000,
            limit_monthly_credits_micro: 600_000_000,
        },
        premium: TierLimits {
            limit_daily_credits_micro: 22_000_000,
            limit_monthly_credits_micro: 300_000_000,
        },
    }
}

fn req(model: &str) -> PreflightRequest<'_> {
    PreflightRequest {
        selected_model: model,
        message_bytes: 1000,
        prior_context_tokens: 0,
        image_count: 0,
        has_ready_documents: false,
        has_ready_code_interpreter: false,
        web_search_requested: false,
    }
}

fn used(spent: i64) -> BucketUsage {
    BucketUsage {
        spent,
        ..BucketUsage::default()
    }
}

#[test]
fn design_example_downgrades_to_standard() {
    // DESIGN §5.10: premium daily 20M spent + 3.75M reserve > 22M -> standard.
    let usage = UsageView {
        total_daily: used(25_000_000),
        total_monthly: used(240_000_000),
        premium_daily: used(20_000_000),
        premium_monthly: used(200_000_000),
    };
    let c = run_cascade(&snapshot(), &limits(), &usage, &req("P"), 32_768).expect("standard available");
    assert_eq!(c.effective.id, "S");
    assert!(c.downgrade);
    assert_eq!(c.reason, Some("premium_quota_exhausted"));
    assert_eq!(c.reserve.reserved_credits_micro, 1_500_000);
}

#[test]
fn allow_when_premium_fits() {
    let c = run_cascade(&snapshot(), &limits(), &UsageView::default(), &req("P"), 32_768).expect("ok");
    assert_eq!(c.effective.id, "P");
    assert!(!c.downgrade);
    assert_eq!(c.reserve.reserved_credits_micro, 3_750_000);
}

#[test]
fn reject_when_all_tiers_exhausted_and_standard_never_upgrades() {
    let usage = UsageView {
        total_daily: used(60_000_000),
        ..UsageView::default()
    };
    assert!(run_cascade(&snapshot(), &limits(), &usage, &req("P"), 32_768).is_none());
    assert!(run_cascade(&snapshot(), &limits(), &usage, &req("S"), 32_768).is_none());
}

#[test]
fn kill_switches_and_disabled_model_downgrade() {
    let mut s = snapshot();
    s.kill_switches.force_standard_tier = true;
    let c = run_cascade(&s, &limits(), &UsageView::default(), &req("P"), 32_768).expect("ok");
    assert_eq!((c.effective.id.as_str(), c.reason), ("S", Some("force_standard_tier")));
    let mut s = snapshot();
    s.kill_switches.disable_premium_tier = true;
    let c = run_cascade(&s, &limits(), &UsageView::default(), &req("P"), 32_768).expect("ok");
    assert_eq!(c.reason, Some("disable_premium_tier"));
    let mut s = snapshot();
    s.model_catalog[0].enabled = false;
    let c = run_cascade(&s, &limits(), &UsageView::default(), &req("P"), 32_768).expect("ok");
    assert_eq!((c.effective.id.as_str(), c.reason), ("S", Some("model_disabled")));
}

#[test]
fn surcharges_follow_tool_support_and_images() {
    let s = snapshot();
    let mut r = req("S");
    r.web_search_requested = true;
    r.image_count = 2;
    let c = candidate_reserve(&s.model_catalog[1], &r, &s, 32_768);
    assert!(c.gates.web_search);
    assert_eq!(c.estimated_input_tokens, 1000 + 200);
}

#[test]
fn settlement_actual_estimated_and_overshoot_cap() {
    let r = TurnReserve {
        tenant_id: Uuid::nil(),
        user_id: Uuid::nil(),
        reserve_tokens: 1500,
        max_output_tokens_applied: 500,
        reserved_credits_micro: 1_500_000,
        minimal_generation_floor_applied: 50,
        premium: false,
        in_mult: 1_000_000_000,
        out_mult: 1_000_000_000,
        daily_start: NaiveDate::MIN,
        monthly_start: NaiveDate::MIN,
    };
    let actual = UsageTokens {
        input_tokens: 900,
        output_tokens: 300,
        ..UsageTokens::default()
    };
    let res = committed_credits(&r, SettlementMethod::Actual(actual), 1.1).expect("ok");
    assert_eq!(res.committed_credits_micro, 1_200_000);
    let est = committed_credits(&r, SettlementMethod::Estimated, 1.1).expect("ok");
    assert_eq!(est.committed_credits_micro, 1_050_000);
    let big = UsageTokens {
        input_tokens: 2000,
        output_tokens: 0,
        ..UsageTokens::default()
    };
    let capped = committed_credits(&r, SettlementMethod::Actual(big), 1.1).expect("ok");
    assert!(capped.overshoot_capped);
    assert_eq!(capped.committed_credits_micro, 1_500_000);
    let within = UsageTokens {
        input_tokens: 1600,
        output_tokens: 0,
        ..UsageTokens::default()
    };
    let w = committed_credits(&r, SettlementMethod::Actual(within), 1.1).expect("ok");
    assert!(!w.overshoot_capped);
    assert_eq!(w.committed_credits_micro, 1_600_000);
    assert_eq!(
        committed_credits(&r, SettlementMethod::Released, 1.1)
            .expect("ok")
            .committed_credits_micro,
        0
    );
}

#[test]
fn status_flags_and_omitted_periods() {
    let mut l = limits();
    l.premium.limit_monthly_credits_micro = 0;
    let usage = UsageView {
        total_daily: used(54_000_000),
        premium_daily: used(22_000_000),
        ..UsageView::default()
    };
    let d = NaiveDate::from_ymd_opt(2026, 1, 15).expect("date");
    let m = NaiveDate::from_ymd_opt(2026, 1, 1).expect("date");
    let st = quota_status(&usage, &l, d, m, 80);
    assert_eq!(st[0].tier, "premium");
    assert_eq!(st[0].periods.len(), 1, "limit <= 0 period omitted");
    assert!(st[0].periods[0].exhausted);
    let total_daily = &st[1].periods[0];
    assert_eq!(total_daily.remaining_percentage, 10);
    assert!(total_daily.warning);
    assert!(!total_daily.exhausted);
    let w = warnings_from_status(&st);
    assert!(w.iter().all(|e| e.next_reset.is_some() == (e.warning || e.exhausted)));
}
