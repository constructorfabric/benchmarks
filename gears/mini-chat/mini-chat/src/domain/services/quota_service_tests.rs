use mini_chat_sdk::{
    KillSwitches, ModelCatalogEntry, ModelTier, PolicySnapshot, TierLimits, UserLimits,
};
use time::macros::{date, datetime};
use uuid::Uuid;

use super::{
    Bucket, BucketUsage, Evaluation, Period, PeriodStarts, PeriodUsage, QuotaDecision, QuotaParams,
    evaluate, next_reset, period_starts, period_starts_from, period_status,
};
use crate::domain::error::{DomainError, QuotaScope};
use crate::domain::estimation::{ReserveInputs, candidate_reserve};
use crate::domain::test_fixtures::{default_of_tier, disabled, premium, standard};

const PARAMS: QuotaParams = QuotaParams {
    max_output_tokens: 32_768,
    minimal_generation_floor: 50,
    web_search_daily_quota: 75,
    code_interpreter_daily_quota: 50,
};

/// Large limits: (standard / total, premium) daily and monthly.
fn limits() -> UserLimits {
    UserLimits {
        user_id: Uuid::nil(),
        policy_version: 1,
        standard: TierLimits {
            limit_daily_credits_micro: 1_000_000_000,
            limit_monthly_credits_micro: 10_000_000_000,
        },
        premium: TierLimits {
            limit_daily_credits_micro: 500_000_000,
            limit_monthly_credits_micro: 5_000_000_000,
        },
    }
}

fn snap(catalog: Vec<ModelCatalogEntry>, ks: KillSwitches) -> PolicySnapshot {
    PolicySnapshot {
        policy_version: 7,
        model_catalog: catalog,
        kill_switches: ks,
    }
}

/// p1 (premium), s1 (standard, default), s2 (standard).
fn catalog() -> Vec<ModelCatalogEntry> {
    vec![
        premium("p1"),
        default_of_tier(standard("s1")),
        standard("s2"),
    ]
}

fn spent(u: &mut PeriodUsage, p: Period, b: Bucket, spent: i64) {
    u.set(
        p,
        b,
        BucketUsage {
            spent_credits_micro: spent,
            ..BucketUsage::default()
        },
    );
}

fn inputs() -> ReserveInputs {
    ReserveInputs {
        message_bytes: 100,
        ..ReserveInputs::default()
    }
}

fn reserve_of(m: &ModelCatalogEntry) -> i64 {
    candidate_reserve(m, &inputs(), &KillSwitches::default(), 32_768, 50).reserved_credits_micro
}

#[test]
fn cascade_allow_at_selected_tier() {
    let s = snap(catalog(), KillSwitches::default());
    let e = evaluate(
        "p1",
        &s,
        &limits(),
        &inputs(),
        &PeriodUsage::default(),
        &PARAMS,
    )
    .unwrap();
    assert_eq!(e.effective.id, "p1");
    assert_eq!(e.effective_tier, ModelTier::Premium);
    assert_eq!(e.decision, QuotaDecision::Allow);
    assert_eq!(e.downgrade_reason, None);
    assert_eq!(e.plan.reserved_credits_micro, reserve_of(&premium("p1")));

    let e = evaluate(
        "s2",
        &s,
        &limits(),
        &inputs(),
        &PeriodUsage::default(),
        &PARAMS,
    )
    .unwrap();
    assert_eq!(e.effective.id, "s2");
    assert_eq!(e.decision, QuotaDecision::Allow);
}

/// `evaluate` with the default limits and inputs.
fn eval(
    model: &str,
    snapshot: &PolicySnapshot,
    usage: &PeriodUsage,
) -> Result<Evaluation, DomainError> {
    evaluate(model, snapshot, &limits(), &inputs(), usage, &PARAMS)
}

fn usage_with(period: Period, bucket: Bucket, amount: i64) -> PeriodUsage {
    let mut usage = PeriodUsage::default();
    spent(&mut usage, period, bucket, amount);
    usage
}

const TOKENS: DomainError = DomainError::QuotaExceeded(QuotaScope::Tokens);

#[test]
fn cascade_truth_tables() {
    let catalog = snap(catalog(), KillSwitches::default());

    // Premium daily exhausted -> standard default model.
    let premium_daily = usage_with(Period::Daily, Bucket::Premium, 500_000_000);
    let e = eval("p1", &catalog, &premium_daily).unwrap();
    assert_eq!(e.effective.id, "s1");
    assert_eq!(e.effective_tier, ModelTier::Standard);
    assert_eq!(e.decision, QuotaDecision::Downgrade);
    assert_eq!(e.downgrade_reason, Some("premium_quota_exhausted"));
    // The booked reserve is the standard candidate's own.
    assert_eq!(e.plan.reserved_credits_micro, reserve_of(&standard("s1")));

    // Premium monthly exhausted (daily fine) -> standard as well.
    let premium_monthly = usage_with(Period::Monthly, Bucket::Premium, 5_000_000_000);
    let e = eval("p1", &catalog, &premium_monthly).unwrap();
    assert_eq!(e.effective.id, "s1");
    assert_eq!(e.downgrade_reason, Some("premium_quota_exhausted"));

    // Standard monthly exhausted -> reject with tokens scope ...
    let total_monthly = usage_with(Period::Monthly, Bucket::Total, 10_000_000_000);
    assert_eq!(eval("s1", &catalog, &total_monthly).unwrap_err(), TOKENS);
    // ... and premium is unavailable too: `total` caps every tier.
    assert_eq!(eval("p1", &catalog, &total_monthly).unwrap_err(), TOKENS);
}

#[test]
fn cascade_premium_kill_switches() {
    let none = PeriodUsage::default();
    let forced = snap(
        catalog(),
        KillSwitches {
            force_standard_tier: true,
            disable_premium_tier: true,
            ..KillSwitches::default()
        },
    );
    let e = eval("p1", &forced, &none).unwrap();
    assert_eq!(e.effective.id, "s1");
    assert_eq!(e.decision, QuotaDecision::Downgrade);
    assert_eq!(e.downgrade_reason, Some("force_standard_tier"));

    let no_premium = snap(
        catalog(),
        KillSwitches {
            disable_premium_tier: true,
            ..KillSwitches::default()
        },
    );
    let e = eval("p1", &no_premium, &none).unwrap();
    assert_eq!(e.downgrade_reason, Some("disable_premium_tier"));

    // A standard selection is not affected by premium kill switches.
    let e = eval("s2", &forced, &none).unwrap();
    assert_eq!(e.decision, QuotaDecision::Allow);
    assert_eq!(e.downgrade_reason, None);
}

#[test]
fn cascade_disabled_or_unknown_selection() {
    let none = PeriodUsage::default();
    // Disabled selected model -> other model of its tier, reason model_disabled.
    let with_p2 = snap(
        vec![disabled(premium("p1")), premium("p2"), standard("s1")],
        KillSwitches::default(),
    );
    let e = eval("p1", &with_p2, &none).unwrap();
    assert_eq!(e.effective.id, "p2");
    assert_eq!(e.effective_tier, ModelTier::Premium);
    assert_eq!(e.decision, QuotaDecision::Downgrade);
    assert_eq!(e.downgrade_reason, Some("model_disabled"));

    // The first reason wins over a later premium exhaustion.
    let premium_daily = usage_with(Period::Daily, Bucket::Premium, 500_000_000);
    let e = eval("p1", &with_p2, &premium_daily).unwrap();
    assert_eq!(e.effective.id, "s1");
    assert_eq!(e.downgrade_reason, Some("model_disabled"));

    // Unknown selected model: starts at premium, reason model_disabled.
    let e = eval("gone", &snap(catalog(), KillSwitches::default()), &none).unwrap();
    assert_eq!(e.effective.id, "p1");
    assert_eq!(e.downgrade_reason, Some("model_disabled"));
    assert_eq!(e.decision, QuotaDecision::Downgrade);
}

#[test]
fn standard_never_upgrades() {
    // No enabled standard model: the premium model is not used.
    let only_premium = snap(
        vec![premium("p1"), disabled(standard("s1"))],
        KillSwitches::default(),
    );
    assert_eq!(
        eval("s1", &only_premium, &PeriodUsage::default()).unwrap_err(),
        TOKENS
    );
}

#[test]
fn candidate_reserve_larger_than_remaining_is_unavailable() {
    let catalog = snap(catalog(), KillSwitches::default());
    let reserve = reserve_of(&premium("p1"));
    let premium_used = |reserved: i64| {
        let mut usage = PeriodUsage::default();
        usage.set(
            Period::Daily,
            Bucket::Premium,
            BucketUsage {
                spent_credits_micro: 500_000_000 - reserve - 10,
                reserved_credits_micro: reserved,
                ..BucketUsage::default()
            },
        );
        usage
    };

    // Exactly fits (spent + reserved + reserve == limit) -> available.
    let e = eval("p1", &catalog, &premium_used(10)).unwrap();
    assert_eq!(e.effective.id, "p1");

    // One micro-credit short -> premium unavailable, downgrade.
    let e = eval("p1", &catalog, &premium_used(11)).unwrap();
    assert_eq!(e.effective.id, "s1");
    assert_eq!(e.downgrade_reason, Some("premium_quota_exhausted"));
}

#[test]
fn premium_candidate_checks_total_bucket() {
    // A cheaper standard candidate still fits where the premium reserve
    // does not fit the `total` bucket.
    let mut cheap = default_of_tier(standard("s1"));
    cheap.output_tokens_credit_multiplier_micro = 1_000_000;
    let cheap_reserve = reserve_of(&cheap);
    assert!(cheap_reserve < reserve_of(&premium("p1")));
    let catalog = snap(vec![premium("p1"), cheap], KillSwitches::default());
    let usage = usage_with(
        Period::Monthly,
        Bucket::Total,
        10_000_000_000 - cheap_reserve,
    );

    let e = eval("p1", &catalog, &usage).unwrap();
    assert_eq!(e.effective.id, "s1");
    assert_eq!(e.downgrade_reason, Some("premium_quota_exhausted"));
    assert_eq!(e.plan.reserved_credits_micro, cheap_reserve);
}

#[test]
fn uncomputable_reserve_is_unavailable() {
    let mut zero = premium("p1");
    zero.output_tokens_credit_multiplier_micro = 0;
    let catalog = snap(
        vec![zero, default_of_tier(standard("s1"))],
        KillSwitches::default(),
    );
    let e = eval("p1", &catalog, &PeriodUsage::default()).unwrap();
    assert_eq!(e.effective.id, "s1");
    assert_eq!(e.downgrade_reason, Some("premium_quota_exhausted"));
}

#[test]
fn default_of_tier_selection() {
    let l = limits();
    let mut u = PeriodUsage::default();
    spent(&mut u, Period::Daily, Bucket::Premium, 500_000_000);

    // Default of the tier wins over catalog order.
    let s = snap(
        vec![
            premium("p1"),
            standard("s-first"),
            default_of_tier(standard("s-default")),
        ],
        KillSwitches::default(),
    );
    let e = evaluate("p1", &s, &l, &inputs(), &u, &PARAMS).unwrap();
    assert_eq!(e.effective.id, "s-default");

    // A disabled default is skipped; then the first enabled model.
    let s = snap(
        vec![
            premium("p1"),
            disabled(default_of_tier(standard("s-default"))),
            standard("s-first"),
            standard("s-second"),
        ],
        KillSwitches::default(),
    );
    let e = evaluate("p1", &s, &l, &inputs(), &u, &PARAMS).unwrap();
    assert_eq!(e.effective.id, "s-first");
}

#[test]
fn daily_web_search_quota_only_when_tool_sent() {
    let s = snap(catalog(), KillSwitches::default());
    let l = limits();
    let mut u = PeriodUsage::default();
    u.set(
        Period::Daily,
        Bucket::Total,
        BucketUsage {
            web_search_calls: 75,
            ..BucketUsage::default()
        },
    );
    let ws = ReserveInputs {
        web_search_requested: true,
        ..inputs()
    };

    assert_eq!(
        evaluate("s1", &s, &l, &ws, &u, &PARAMS).unwrap_err(),
        DomainError::QuotaExceeded(QuotaScope::WebSearch)
    );
    // Not requested -> not checked.
    assert!(evaluate("s1", &s, &l, &inputs(), &u, &PARAMS).is_ok());
    // Model without web search -> no tool, not checked.
    let mut no_ws = default_of_tier(standard("s1"));
    no_ws.general_config.tool_support.web_search = false;
    let s2 = snap(vec![no_ws], KillSwitches::default());
    assert!(evaluate("s1", &s2, &l, &ws, &u, &PARAMS).is_ok());
    // Below the quota -> OK.
    u.set(
        Period::Daily,
        Bucket::Total,
        BucketUsage {
            web_search_calls: 74,
            ..BucketUsage::default()
        },
    );
    assert!(evaluate("s1", &s, &l, &ws, &u, &PARAMS).is_ok());
}

#[test]
fn daily_code_interpreter_quota_only_when_tool_sent() {
    let l = limits();
    let mut u = PeriodUsage::default();
    u.set(
        Period::Daily,
        Bucket::Total,
        BucketUsage {
            code_interpreter_calls: 50,
            ..BucketUsage::default()
        },
    );
    let xlsx = ReserveInputs {
        chat_has_ready_xlsx: true,
        ..inputs()
    };
    let s = snap(catalog(), KillSwitches::default());

    assert_eq!(
        evaluate("s1", &s, &l, &xlsx, &u, &PARAMS).unwrap_err(),
        DomainError::QuotaExceeded(QuotaScope::CodeInterpreter)
    );
    // No ready XLSX -> no tool.
    assert!(evaluate("s1", &s, &l, &inputs(), &u, &PARAMS).is_ok());
    // Kill switch -> no tool.
    let ks = snap(
        catalog(),
        KillSwitches {
            disable_code_interpreter: true,
            ..KillSwitches::default()
        },
    );
    assert!(evaluate("s1", &ks, &l, &xlsx, &u, &PARAMS).is_ok());
    // Model without code interpreter -> no tool.
    let mut no_ci = default_of_tier(standard("s1"));
    no_ci.general_config.tool_support.code_interpreter = false;
    let s2 = snap(vec![no_ci], KillSwitches::default());
    assert!(evaluate("s1", &s2, &l, &xlsx, &u, &PARAMS).is_ok());
}

#[test]
fn periods_are_utc_dates() {
    assert_eq!(
        period_starts(datetime!(2026-02-28 15:30:00 UTC)),
        PeriodStarts {
            daily: date!(2026 - 02 - 28),
            monthly: date!(2026 - 02 - 01),
        }
    );
    assert_eq!(
        period_starts(datetime!(2026-02-28 23:59:59 UTC)).daily,
        date!(2026 - 02 - 28)
    );
    assert_eq!(
        period_starts(datetime!(2026-03-01 00:00:00 UTC)),
        PeriodStarts {
            daily: date!(2026 - 03 - 01),
            monthly: date!(2026 - 03 - 01),
        }
    );
    // Non-UTC offsets are converted first.
    assert_eq!(
        period_starts(datetime!(2026-03-01 01:30:00 +02:00)),
        PeriodStarts {
            daily: date!(2026 - 02 - 28),
            monthly: date!(2026 - 02 - 01),
        }
    );
    assert_eq!(
        period_starts_from(datetime!(2026-12-31 23:59:59 UTC)),
        PeriodStarts {
            daily: date!(2026 - 12 - 31),
            monthly: date!(2026 - 12 - 01),
        }
    );
}

#[test]
fn next_reset_is_next_period_midnight_utc() {
    let now = datetime!(2026-12-31 10:00:00 UTC);
    assert_eq!(
        next_reset(Period::Daily, now),
        datetime!(2027-01-01 00:00:00 UTC)
    );
    assert_eq!(
        next_reset(Period::Monthly, now),
        datetime!(2027-01-01 00:00:00 UTC)
    );
    let now = datetime!(2026-02-10 00:00:00 UTC);
    assert_eq!(
        next_reset(Period::Daily, now),
        datetime!(2026-02-11 00:00:00 UTC)
    );
    assert_eq!(
        next_reset(Period::Monthly, now),
        datetime!(2026-03-01 00:00:00 UTC)
    );
}

#[test]
fn period_status_math() {
    let now = datetime!(2026-10-04 12:00:00 UTC);
    let used = |spent, reserved| BucketUsage {
        spent_credits_micro: spent,
        reserved_credits_micro: reserved,
        ..BucketUsage::default()
    };

    let p = period_status(Period::Daily, 1_000, &used(700, 100), now, 80).unwrap();
    assert_eq!(p.limit_credits_micro, 1_000);
    assert_eq!(p.used_credits_micro, 800);
    assert_eq!(p.remaining_credits_micro, 200);
    assert_eq!(p.remaining_percentage, 20);
    assert!(p.warning); // 20 <= 100 - 80
    assert!(!p.exhausted);
    assert_eq!(p.next_reset, datetime!(2026-10-05 00:00:00 UTC));

    let p = period_status(Period::Daily, 1_000, &used(790, 0), now, 80).unwrap();
    assert_eq!(p.remaining_percentage, 21);
    assert!(!p.warning);

    // Floored: 0.9% remaining is already exhausted.
    let p = period_status(Period::Monthly, 1_000, &used(991, 0), now, 80).unwrap();
    assert_eq!(p.remaining_percentage, 0);
    assert!(p.exhausted && p.warning);
    assert_eq!(p.next_reset, datetime!(2026-11-01 00:00:00 UTC));

    // Over the limit: remaining clamps at 0.
    let p = period_status(Period::Daily, 1_000, &used(900, 500), now, 80).unwrap();
    assert_eq!(p.used_credits_micro, 1_400);
    assert_eq!(p.remaining_credits_micro, 0);
    assert!(p.exhausted);

    // No overflow on huge limits.
    let p = period_status(Period::Daily, i64::MAX, &used(0, 0), now, 80).unwrap();
    assert_eq!(p.remaining_percentage, 100);

    // limit <= 0 -> omitted.
    assert!(period_status(Period::Daily, 0, &used(0, 0), now, 80).is_none());
    assert!(period_status(Period::Daily, -5, &used(0, 0), now, 80).is_none());
}
