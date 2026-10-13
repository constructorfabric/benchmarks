#![allow(clippy::unwrap_used)]

use mini_chat_sdk::{KillSwitches, ModelCatalogEntry, PolicySnapshot, TierLimits, UserLimits};
use serde_json::json;
use time::macros::datetime;
use uuid::Uuid;

use super::{
    BucketUsage, PeriodStarts, RequestFacts, UsageSnapshot, candidate_reserve, cascade, next_reset,
    status_entries, tier_available,
};

fn model(id: &str, tier: &str, enabled: bool, mult: i64, ts: &serde_json::Value) -> ModelCatalogEntry {
    serde_json::from_value(json!({
        "id": id, "provider_model_id": id, "display_name": id, "provider_id": "p", "tier": tier,
        "enabled": enabled, "context_window": 100_000, "max_output_tokens": 500, "max_input_tokens": 0,
        "input_tokens_credit_multiplier_micro": mult, "output_tokens_credit_multiplier_micro": mult,
        "max_num_results": 5,
        "estimation_budgets": {"bytes_per_token_conservative": 1, "fixed_overhead_tokens": 0, "safety_margin_pct": 0,
            "image_token_budget": 1000, "tool_surcharge_tokens": 500, "web_search_surcharge_tokens": 300,
            "code_interpreter_surcharge_tokens": 700},
        "general_config": {"tool_support": ts}
    }))
    .unwrap()
}

fn snap(models: Vec<ModelCatalogEntry>, ks: KillSwitches) -> PolicySnapshot {
    PolicySnapshot { policy_version: 1, model_catalog: models, kill_switches: ks }
}

fn limits() -> UserLimits {
    UserLimits {
        user_id: Uuid::nil(),
        policy_version: 1,
        standard: TierLimits { limit_daily_credits_micro: 60_000_000, limit_monthly_credits_micro: 600_000_000 },
        premium: TierLimits { limit_daily_credits_micro: 22_000_000, limit_monthly_credits_micro: 300_000_000 },
    }
}

fn facts_1000() -> RequestFacts {
    RequestFacts { message_bytes: 1000, ..RequestFacts::default() }
}

fn b(spent: i64) -> BucketUsage {
    BucketUsage { spent, ..BucketUsage::default() }
}

fn catalog() -> Vec<ModelCatalogEntry> {
    vec![
        model("P", "premium", true, 2_500_000_000, &json!({})),
        model("S", "standard", true, 1_000_000_000, &json!({})),
    ]
}

#[test]
fn design_example_downgrades_premium_to_standard() {
    // §5.10: premium reserve 3.75M does not fit the premium daily subcap
    let usage = UsageSnapshot {
        daily_total: b(25_000_000),
        monthly_total: b(240_000_000),
        daily_premium: b(20_000_000),
        monthly_premium: b(200_000_000),
    };
    let d = cascade(&snap(catalog(), KillSwitches::default()), &limits(), &usage, "P", &facts_1000(), 32768).unwrap();
    assert_eq!(d.effective.id, "S");
    assert!(d.downgraded);
    assert_eq!(d.downgrade_reason, Some("premium_quota_exhausted"));
    assert_eq!(d.reserve.reserved_credits_micro, 1_500_000);
    assert_eq!(d.reserve.reserve_tokens, 1_500);
}

#[test]
fn allow_when_premium_fits_and_reject_when_all_exhausted() {
    let d = cascade(&snap(catalog(), KillSwitches::default()), &limits(), &UsageSnapshot::default(), "P", &facts_1000(), 32768).unwrap();
    assert_eq!(d.effective.id, "P");
    assert!(!d.downgraded);
    assert_eq!(d.downgrade_reason, None);
    let full = UsageSnapshot { daily_total: b(60_000_000), ..UsageSnapshot::default() };
    assert!(cascade(&snap(catalog(), KillSwitches::default()), &limits(), &full, "P", &facts_1000(), 32768).is_none());
    // standard selected never upgrades to premium
    let full_std = UsageSnapshot { monthly_total: b(600_000_000), ..UsageSnapshot::default() };
    assert!(cascade(&snap(catalog(), KillSwitches::default()), &limits(), &full_std, "S", &facts_1000(), 32768).is_none());
}

#[test]
fn kill_switches_and_disabled_models() {
    let ks = KillSwitches { force_standard_tier: true, ..KillSwitches::default() };
    let d = cascade(&snap(catalog(), ks), &limits(), &UsageSnapshot::default(), "P", &facts_1000(), 32768).unwrap();
    assert_eq!((d.effective.id.as_str(), d.downgrade_reason), ("S", Some("force_standard_tier")));
    let ks = KillSwitches { disable_premium_tier: true, ..KillSwitches::default() };
    let d = cascade(&snap(catalog(), ks), &limits(), &UsageSnapshot::default(), "P", &facts_1000(), 32768).unwrap();
    assert_eq!(d.downgrade_reason, Some("disable_premium_tier"));
    let mut cat = catalog();
    cat[0].enabled = false;
    let d = cascade(&snap(cat, KillSwitches::default()), &limits(), &UsageSnapshot::default(), "P", &facts_1000(), 32768).unwrap();
    assert_eq!((d.effective.id.as_str(), d.downgrade_reason), ("S", Some("model_disabled")));
    let d = cascade(&snap(catalog(), KillSwitches::default()), &limits(), &UsageSnapshot::default(), "gone", &facts_1000(), 32768).unwrap();
    assert_eq!((d.effective.id.as_str(), d.downgrade_reason), ("P", Some("model_disabled")));
    // no enabled model in any tier -> reject
    let mut cat = catalog();
    cat[0].enabled = false;
    cat[1].enabled = false;
    assert!(cascade(&snap(cat, KillSwitches::default()), &limits(), &UsageSnapshot::default(), "P", &facts_1000(), 32768).is_none());
}

#[test]
fn zero_multiplier_candidate_is_unavailable() {
    let mut cat = catalog();
    cat[0].input_tokens_credit_multiplier_micro = 0;
    let d = cascade(&snap(cat, KillSwitches::default()), &limits(), &UsageSnapshot::default(), "P", &facts_1000(), 32768).unwrap();
    assert_eq!(d.effective.id, "S");
}

#[test]
fn surcharges_follow_tool_support_and_kill_switches() {
    let m = model("M", "standard", true, 1_000_000, &json!({"web_search": true, "file_search": true, "code_interpreter": true}));
    let facts = RequestFacts {
        message_bytes: 0, prior_context_tokens: 40, image_count: 2, has_ready_docs: true, has_ready_xlsx: true,
        web_search_requested: true,
    };
    let r = candidate_reserve(&m, &facts, KillSwitches::default(), 32768);
    // 0 text + 40 prior + 2000 images + 500 + 300 + 700
    assert_eq!(r.estimated_input_tokens, 3540);
    assert_eq!(r.max_output_tokens_applied, 500);
    assert!(r.tools.file_search && r.tools.web_search && r.tools.code_interpreter);
    let ks = KillSwitches { disable_file_search: true, disable_code_interpreter: true, ..KillSwitches::default() };
    let r = candidate_reserve(&m, &facts, ks, 100);
    assert_eq!(r.estimated_input_tokens, 2340);
    assert_eq!(r.max_output_tokens_applied, 100);
    let plain = model("N", "standard", true, 1_000_000, &json!({}));
    let r = candidate_reserve(&plain, &facts, KillSwitches::default(), 32768);
    assert_eq!(r.estimated_input_tokens, 2040);
}

#[test]
fn tier_availability_checks_both_buckets_for_premium() {
    let usage = UsageSnapshot { daily_premium: b(21_999_999), ..UsageSnapshot::default() };
    assert!(tier_available(mini_chat_sdk::ModelTier::Premium, &usage, &limits(), 1));
    assert!(!tier_available(mini_chat_sdk::ModelTier::Premium, &usage, &limits(), 2));
    assert!(tier_available(mini_chat_sdk::ModelTier::Standard, &usage, &limits(), 2));
}

#[test]
fn periods_and_resets_are_utc() {
    let p = PeriodStarts::of(datetime!(2026-02-28 23:59:59 UTC));
    assert_eq!(p.daily.to_string(), "2026-02-28");
    assert_eq!(p.monthly.to_string(), "2026-02-01");
    assert_eq!(next_reset("daily", datetime!(2026-02-28 23:59:59 UTC)), datetime!(2026-03-01 00:00:00 UTC));
    assert_eq!(next_reset("monthly", datetime!(2026-12-15 10:00:00 UTC)), datetime!(2027-01-01 00:00:00 UTC));
}

#[test]
fn status_flags() {
    let usage = UsageSnapshot {
        daily_total: BucketUsage { spent: 50_000_000, reserved: 1_000_000, ..BucketUsage::default() },
        daily_premium: b(22_000_000),
        ..UsageSnapshot::default()
    };
    let mut l = limits();
    l.premium.limit_monthly_credits_micro = 0;
    let s = status_entries(&usage, &l, 80, datetime!(2026-05-05 12:00:00 UTC));
    assert_eq!(s.len(), 3);
    let pd = &s[0];
    assert_eq!((pd.tier, pd.period, pd.remaining_percentage, pd.exhausted, pd.warning), ("premium", "daily", 0, true, true));
    let td = s.iter().find(|e| e.tier == "total" && e.period == "daily").unwrap();
    assert_eq!(td.used, 51_000_000);
    assert_eq!(td.remaining_percentage, 15);
    assert!(td.warning && !td.exhausted);
    let tm = s.iter().find(|e| e.tier == "total" && e.period == "monthly").unwrap();
    assert!(!tm.warning);
}
