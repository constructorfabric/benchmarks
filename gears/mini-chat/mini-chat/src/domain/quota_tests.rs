use mini_chat_sdk::TierLimits;
use time::macros::datetime;

use super::*;

fn entry(id: &str, tier: &str, enabled: bool, is_default: bool, web: bool) -> ModelCatalogEntry {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "provider_model_id": format!("{id}-p"),
        "display_name": id,
        "provider_id": "openai",
        "provider_display_name": "OpenAI",
        "tier": tier,
        "enabled": enabled,
        "context_window": 128_000,
        "max_output_tokens": 1_000,
        "max_input_tokens": 0,
        "input_tokens_credit_multiplier_micro": if tier == "premium" { 3_000_000 } else { 1_000_000 },
        "output_tokens_credit_multiplier_micro": if tier == "premium" { 15_000_000 } else { 2_000_000 },
        "max_num_results": 5,
        "general_config": {"tool_support": {"web_search": web, "file_search": true, "code_interpreter": true}},
        "preference": {"is_default": is_default, "sort_order": 0}
    }))
    .expect("entry")
}

fn snapshot(ks: KillSwitches) -> PolicySnapshot {
    PolicySnapshot {
        policy_version: 7,
        model_catalog: vec![
            entry("prem", "premium", true, true, true),
            entry("prem-off", "premium", false, false, true),
            entry("std", "standard", true, true, false),
        ],
        kill_switches: ks,
    }
}

fn limits(std_daily: i64, prem_daily: i64) -> UserLimits {
    UserLimits {
        user_id: Uuid::nil(),
        policy_version: 7,
        standard: TierLimits {
            limit_daily_credits_micro: std_daily,
            limit_monthly_credits_micro: std_daily * 30,
        },
        premium: TierLimits {
            limit_daily_credits_micro: prem_daily,
            limit_monthly_credits_micro: prem_daily * 30,
        },
    }
}

fn input<'a>(
    snap: &'a PolicySnapshot,
    lim: &'a UserLimits,
    usage: &'a UsageMap,
    selected: &'a str,
) -> PreflightInput<'a> {
    PreflightInput {
        snapshot: snap,
        limits: lim,
        selected_model: selected,
        message_text: "hello",
        prior_context_tokens: 0,
        image_count: 0,
        chat_has_ready_docs: false,
        chat_has_ready_xlsx: false,
        web_search_requested: false,
        usage,
        max_output_cap: 32_768,
        minimal_generation_floor: 50,
        web_search_daily_quota: 75,
        code_interpreter_daily_quota: 50,
        periods: Periods::at(datetime!(2026-03-15 10:00 UTC)),
    }
}

#[test]
fn periods_are_utc_calendar() {
    let p = Periods::at(datetime!(2026-03-15 23:30 -02:00));
    assert_eq!(p.daily, time::macros::date!(2026 - 03 - 16));
    assert_eq!(p.monthly, time::macros::date!(2026 - 03 - 01));
}

#[test]
fn next_reset_daily_and_monthly() {
    let now = datetime!(2026-12-31 15:00 UTC);
    assert_eq!(next_reset(DAILY, now), datetime!(2027-01-01 0:00 UTC));
    assert_eq!(next_reset(MONTHLY, now), datetime!(2027-01-01 0:00 UTC));
    let now = datetime!(2026-02-10 15:00 UTC);
    assert_eq!(next_reset(MONTHLY, now), datetime!(2026-03-01 0:00 UTC));
}

#[test]
fn status_warning_and_exhaustion() {
    let lim = limits(1_000, 500);
    let mut usage = UsageMap::new();
    usage.insert(
        (DAILY.to_owned(), BUCKET_TOTAL.to_owned()),
        BucketUsage {
            spent: 700,
            reserved: 110,
            ..BucketUsage::default()
        },
    );
    usage.insert(
        (DAILY.to_owned(), BUCKET_PREMIUM.to_owned()),
        BucketUsage {
            spent: 600,
            ..BucketUsage::default()
        },
    );
    let s = compute_status(&lim, &usage, datetime!(2026-03-15 10:00 UTC), 80);
    let total = s.iter().find(|t| t.tier == "total").unwrap();
    let d = total.periods.iter().find(|p| p.period == DAILY).unwrap();
    assert_eq!((d.used, d.remaining, d.remaining_pct), (810, 190, 19));
    assert!(d.warning && !d.exhausted);
    let prem = s.iter().find(|t| t.tier == "premium").unwrap();
    let pd = prem.periods.iter().find(|p| p.period == DAILY).unwrap();
    assert_eq!((pd.remaining, pd.remaining_pct), (0, 0));
    assert!(pd.exhausted && pd.warning);
    let m = total.periods.iter().find(|p| p.period == MONTHLY).unwrap();
    assert!(!m.warning && m.remaining_pct == 100);
}

#[test]
fn status_skips_periods_without_limit() {
    let lim = limits(0, 500);
    let s = compute_status(&lim, &UsageMap::new(), datetime!(2026-03-15 10:00 UTC), 80);
    assert!(
        s.iter()
            .find(|t| t.tier == "total")
            .unwrap()
            .periods
            .is_empty()
    );
}

#[test]
fn preflight_allows_selected_model() {
    let snap = snapshot(KillSwitches::default());
    let lim = limits(100_000_000, 50_000_000);
    let usage = UsageMap::new();
    let d = preflight(&input(&snap, &lim, &usage, "prem")).unwrap();
    assert_eq!(d.effective.id, "prem");
    assert_eq!(d.decision, QuotaDecision::Allow);
    assert!(d.premium);
    assert_eq!(d.max_output_tokens_applied, 1_000);
    assert_eq!(d.reserve_tokens, d.estimated_input_tokens + 1_000);
    assert_eq!(
        d.reserved_credits_micro,
        credits::credits_micro(d.estimated_input_tokens, 1_000, 3_000_000, 15_000_000).unwrap()
    );
    assert_eq!(d.minimal_generation_floor_applied, 50);
    assert_eq!(d.policy_version, 7);
}

#[test]
fn preflight_downgrades_when_premium_exhausted() {
    let snap = snapshot(KillSwitches::default());
    let lim = limits(100_000_000, 1_000);
    let usage = UsageMap::new();
    let d = preflight(&input(&snap, &lim, &usage, "prem")).unwrap();
    assert_eq!(d.effective.id, "std");
    assert_eq!(d.decision, QuotaDecision::Downgrade);
    assert_eq!(
        d.downgrade_reason.as_deref(),
        Some("premium_quota_exhausted")
    );
    assert!(!d.premium);
}

#[test]
fn preflight_kill_switch_reasons() {
    let lim = limits(100_000_000, 50_000_000);
    let usage = UsageMap::new();
    for (ks, reason) in [
        (
            KillSwitches {
                force_standard_tier: true,
                ..KillSwitches::default()
            },
            "force_standard_tier",
        ),
        (
            KillSwitches {
                disable_premium_tier: true,
                ..KillSwitches::default()
            },
            "disable_premium_tier",
        ),
    ] {
        let snap = snapshot(ks);
        let d = preflight(&input(&snap, &lim, &usage, "prem")).unwrap();
        assert_eq!(d.effective.id, "std");
        assert_eq!(d.downgrade_reason.as_deref(), Some(reason));
    }
}

#[test]
fn preflight_disabled_or_missing_selected_model() {
    let snap = snapshot(KillSwitches::default());
    let lim = limits(100_000_000, 50_000_000);
    let usage = UsageMap::new();
    for selected in ["prem-off", "gone"] {
        let d = preflight(&input(&snap, &lim, &usage, selected)).unwrap();
        assert_eq!(d.effective.id, "prem");
        assert_eq!(d.decision, QuotaDecision::Downgrade);
        assert_eq!(d.downgrade_reason.as_deref(), Some("model_disabled"));
    }
}

#[test]
fn preflight_standard_never_upgrades() {
    let snap = snapshot(KillSwitches::default());
    let lim = limits(1_000, 50_000_000);
    let usage = UsageMap::new();
    let err = preflight(&input(&snap, &lim, &usage, "std")).unwrap_err();
    assert!(matches!(
        err,
        DomainError::QuotaExceeded(QuotaScope::Tokens)
    ));
}

#[test]
fn preflight_counts_reserved_credits() {
    let snap = snapshot(KillSwitches::default());
    let lim = limits(100_000, 100_000);
    let mut usage = UsageMap::new();
    usage.insert(
        (MONTHLY.to_owned(), BUCKET_TOTAL.to_owned()),
        BucketUsage {
            reserved: 3_000_000,
            ..BucketUsage::default()
        },
    );
    assert!(preflight(&input(&snap, &lim, &usage, "prem")).is_err());
}

#[test]
fn web_search_daily_quota_only_when_tool_sent() {
    let snap = snapshot(KillSwitches::default());
    let lim = limits(100_000_000, 50_000_000);
    let mut usage = UsageMap::new();
    usage.insert(
        (DAILY.to_owned(), BUCKET_TOTAL.to_owned()),
        BucketUsage {
            web_search_calls: 75,
            ..BucketUsage::default()
        },
    );
    let mut i = input(&snap, &lim, &usage, "prem");
    i.web_search_requested = true;
    let err = preflight(&i).unwrap_err();
    assert!(matches!(
        err,
        DomainError::QuotaExceeded(QuotaScope::WebSearch)
    ));
    // a model without web search support sends no tool and skips the check
    let mut i = input(&snap, &lim, &usage, "std");
    i.web_search_requested = true;
    let d = preflight(&i).unwrap();
    assert!(!d.tools.web_search);
}

#[test]
fn tools_follow_support_and_kill_switches() {
    let snap = snapshot(KillSwitches::default());
    let lim = limits(1, 1);
    let usage = UsageMap::new();
    let mut i = input(&snap, &lim, &usage, "prem");
    i.chat_has_ready_docs = true;
    i.chat_has_ready_xlsx = true;
    i.web_search_requested = true;
    let m = snap.find("prem").unwrap();
    assert_eq!(
        tools_for(m, KillSwitches::default(), &i),
        ToolSet {
            file_search: true,
            web_search: true,
            code_interpreter: true
        }
    );
    let ks = KillSwitches {
        disable_file_search: true,
        disable_code_interpreter: true,
        ..KillSwitches::default()
    };
    assert_eq!(
        tools_for(m, ks, &i),
        ToolSet {
            file_search: false,
            web_search: true,
            code_interpreter: false
        }
    );
}

#[test]
fn surcharges_raise_the_reserve() {
    let snap = snapshot(KillSwitches::default());
    let lim = limits(100_000_000, 50_000_000);
    let usage = UsageMap::new();
    let base = preflight(&input(&snap, &lim, &usage, "prem")).unwrap();
    let mut i = input(&snap, &lim, &usage, "prem");
    i.chat_has_ready_docs = true;
    i.image_count = 1;
    let with = preflight(&i).unwrap();
    let b = &snap.find("prem").unwrap().estimation_budgets;
    assert_eq!(
        with.estimated_input_tokens - base.estimated_input_tokens,
        i64::from(b.tool_surcharge_tokens) + i64::from(b.image_token_budget)
    );
}
