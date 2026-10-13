#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 6: `cpt-cf-mini-chat-algo-quota-billing-quota-view` (status and
//! warnings mode) and the arithmetic deviation rows of the FEATURE.

use mini_chat_sdk::UserLimits;

use super::{QuotaPeriodView, QuotaTierView, QuotaWarningView, build_status_view, build_warnings};
use crate::domain::service::quota::periods::{PeriodKeys, period_keys};
use crate::domain::service::quota::test_support::{limits, utc};
use crate::domain::service::quota::types::QuotaBucket::{TierPremium, Total};
use crate::domain::service::quota::types::QuotaPeriod::{Daily, Monthly};
use crate::domain::service::quota::types::{BucketRow, QuotaPeriod, QuotaTier};

fn keys() -> PeriodKeys {
    period_keys(utc(2026, 2, 28, 23, 59, 59))
}

/// Total limits of `total_daily` / `total_monthly`, premium 1 / 1.
fn total_limits(total_daily: i64, total_monthly: i64) -> UserLimits {
    limits(7, total_daily, total_monthly, 1, 1)
}

fn total_daily(view: &[QuotaTierView]) -> QuotaPeriodView {
    view.iter()
        .find(|tier| tier.tier == QuotaTier::Total)
        .unwrap()
        .periods
        .iter()
        .find(|period| period.period == Daily)
        .copied()
        .unwrap()
}

fn status(limit: i64, spent: i64, reserved: i64, threshold: u8) -> QuotaPeriodView {
    let rows = vec![BucketRow::new(Daily, Total, spent, reserved)];
    total_daily(&build_status_view(
        &total_limits(limit, limit),
        &rows,
        threshold,
        keys(),
    ))
}

// ---- the arithmetic of the deviation rows ------------------------------------------------------------

#[test]
fn eighty_million_and_one_used_of_a_hundred_million_leaves_19_999_999_and_19_percent() {
    let view = status(100_000_000, 80_000_001, 0, 80);

    assert_eq!(view.limit_credits_micro, 100_000_000);
    assert_eq!(view.used_credits_micro, 80_000_001);
    assert_eq!(view.remaining_credits_micro, 19_999_999);
    assert_eq!(view.remaining_percentage, 19);
    assert!(view.warning);
    assert!(!view.exhausted);
}

#[test]
fn ninety_nine_million_and_one_used_is_exhausted_because_less_than_one_percent_remains() {
    let view = status(100_000_000, 99_000_001, 0, 80);

    assert_eq!(view.remaining_credits_micro, 999_999);
    assert_eq!(view.remaining_percentage, 0);
    assert!(view.exhausted);
    assert!(view.warning);
}

#[test]
fn used_above_the_limit_is_not_clamped_but_remaining_is() {
    let view = status(100_000_000, 120_000_000, 5_000_000, 80);

    assert_eq!(view.used_credits_micro, 125_000_000);
    assert_eq!(view.remaining_credits_micro, 0);
    assert_eq!(view.remaining_percentage, 0);
    assert!(view.exhausted);
    assert!(view.warning);
}

#[test]
fn used_is_spent_plus_reserved() {
    let view = status(100_000_000, 50_000_000, 30_000_000, 80);

    assert_eq!(view.used_credits_micro, 80_000_000);
    assert_eq!(view.remaining_credits_micro, 20_000_000);
    assert_eq!(view.remaining_percentage, 20);
}

#[test]
fn a_missing_row_is_zero_spent_and_zero_reserved() {
    let view = total_daily(&build_status_view(
        &total_limits(100_000_000, 100_000_000),
        &[],
        80,
        keys(),
    ));

    assert_eq!(view.used_credits_micro, 0);
    assert_eq!(view.remaining_credits_micro, 100_000_000);
    assert_eq!(view.remaining_percentage, 100);
    assert!(!view.warning);
    assert!(!view.exhausted);
}

#[test]
fn the_warning_fires_when_the_remaining_percentage_reaches_the_threshold_complement() {
    // threshold 80: warning when remaining_percentage <= 20
    assert!(
        status(100_000_000, 80_000_000, 0, 80).warning,
        "20% remaining"
    );
    assert!(
        status(100_000_000, 79_999_999, 0, 80).warning,
        "20.000001% floors to 20"
    );
    assert!(
        !status(100_000_000, 79_000_000, 0, 80).warning,
        "21% remaining"
    );
    // threshold 50: warning when remaining_percentage <= 50
    assert!(status(100_000_000, 50_000_000, 0, 50).warning);
    assert!(!status(100_000_000, 49_000_000, 0, 50).warning);
    // threshold 99: warning when remaining_percentage <= 1
    assert!(status(100_000_000, 99_000_000, 0, 99).warning);
    assert!(!status(100_000_000, 98_000_000, 0, 99).warning);
}

#[test]
fn the_percentage_is_floored_and_computed_in_wide_integers() {
    // 2 of 3 remain: 66.66..% floors to 66
    assert_eq!(status(3, 1, 0, 80).remaining_percentage, 66);
    // i64 arithmetic would overflow on remaining * 100
    let huge = status(i64::MAX, 1, 0, 80);
    assert_eq!(huge.remaining_percentage, 99);
    assert_eq!(huge.remaining_credits_micro, i64::MAX - 1);
    assert_eq!(status(i64::MAX, 0, 0, 80).remaining_percentage, 100);
}

#[test]
fn exhausted_means_a_zero_percentage_only() {
    assert!(!status(100, 99, 0, 80).exhausted, "1% remaining");
    assert!(status(100, 100, 0, 80).exhausted);
    assert!(status(1_000, 991, 0, 80).exhausted, "0.9% floors to 0");
}

// ---- structure, order and omissions ---------------------------------------------------------------------

#[test]
fn the_tiers_are_premium_then_total_and_the_periods_daily_then_monthly() {
    let view = build_status_view(&limits(7, 60, 600, 22, 300), &[], 80, keys());

    assert_eq!(
        view.iter().map(|tier| tier.tier).collect::<Vec<_>>(),
        vec![QuotaTier::Premium, QuotaTier::Total]
    );
    for tier in &view {
        assert_eq!(
            tier.periods.iter().map(|p| p.period).collect::<Vec<_>>(),
            vec![Daily, Monthly]
        );
    }
}

#[test]
fn premium_uses_the_premium_limits_and_bucket_and_total_the_standard_ones() {
    let rows = vec![
        BucketRow::new(Daily, TierPremium, 11, 0),
        BucketRow::new(Monthly, TierPremium, 22, 0),
        BucketRow::new(Daily, Total, 33, 0),
        BucketRow::new(Monthly, Total, 44, 0),
    ];

    let view = build_status_view(&limits(7, 60, 600, 22, 300), &rows, 80, keys());

    let premium = &view[0];
    assert_eq!(premium.tier, QuotaTier::Premium);
    assert_eq!(
        (
            premium.periods[0].limit_credits_micro,
            premium.periods[0].used_credits_micro
        ),
        (22, 11)
    );
    assert_eq!(
        (
            premium.periods[1].limit_credits_micro,
            premium.periods[1].used_credits_micro
        ),
        (300, 22)
    );
    let total = &view[1];
    assert_eq!(total.tier, QuotaTier::Total);
    assert_eq!(
        (
            total.periods[0].limit_credits_micro,
            total.periods[0].used_credits_micro
        ),
        (60, 33)
    );
    assert_eq!(
        (
            total.periods[1].limit_credits_micro,
            total.periods[1].used_credits_micro
        ),
        (600, 44)
    );
}

#[test]
fn a_period_with_a_zero_or_negative_limit_is_omitted() {
    let view = build_status_view(&limits(7, 60, 0, -5, 300), &[], 80, keys());

    let premium = &view[0];
    assert_eq!(premium.tier, QuotaTier::Premium);
    assert_eq!(
        premium.periods.iter().map(|p| p.period).collect::<Vec<_>>(),
        vec![Monthly],
        "the negative daily limit is omitted"
    );
    let total = &view[1];
    assert_eq!(
        total.periods.iter().map(|p| p.period).collect::<Vec<_>>(),
        vec![Daily],
        "the zero monthly limit is omitted"
    );
}

#[test]
fn a_tier_whose_periods_are_all_omitted_is_still_listed_with_an_empty_period_list() {
    let view = build_status_view(&limits(7, 60, 600, 0, 0), &[], 80, keys());

    assert_eq!(view.len(), 2);
    assert_eq!(view[0].tier, QuotaTier::Premium);
    assert!(view[0].periods.is_empty());
    assert_eq!(view[1].periods.len(), 2);
}

#[test]
fn next_reset_is_the_next_utc_midnight_and_the_first_of_the_next_month() {
    let view = build_status_view(&limits(7, 60, 600, 22, 300), &[], 80, keys());

    for tier in &view {
        assert_eq!(tier.periods[0].next_reset, utc(2026, 3, 1, 0, 0, 0));
        assert_eq!(tier.periods[1].next_reset, utc(2026, 3, 1, 0, 0, 0));
    }
    let mid_month = build_status_view(
        &limits(7, 60, 600, 22, 300),
        &[],
        80,
        period_keys(utc(2026, 3, 10, 12, 0, 0)),
    );
    assert_eq!(
        mid_month[1].periods[0].next_reset,
        utc(2026, 3, 11, 0, 0, 0)
    );
    assert_eq!(mid_month[1].periods[1].next_reset, utc(2026, 4, 1, 0, 0, 0));
}

#[test]
fn rows_of_the_other_period_or_bucket_do_not_leak_into_an_entry() {
    let rows = vec![BucketRow::new(Monthly, Total, 50, 0)];

    let view = build_status_view(&total_limits(100, 100), &rows, 80, keys());

    let total = &view[1];
    assert_eq!(total.periods[0].used_credits_micro, 0, "daily total");
    assert_eq!(total.periods[1].used_credits_micro, 50, "monthly total");
    assert_eq!(view[0].periods[0].used_credits_micro, 0, "premium");
}

// ---- warnings mode ------------------------------------------------------------------------------------

#[test]
fn warnings_list_every_tier_and_period_with_a_positive_limit() {
    let warnings = build_warnings(&limits(7, 100, 100, 100, 100), &[], 80, keys());

    let order: Vec<(QuotaTier, QuotaPeriod)> =
        warnings.iter().map(|w| (w.tier, w.period)).collect();
    assert_eq!(
        order,
        vec![
            (QuotaTier::Premium, Daily),
            (QuotaTier::Premium, Monthly),
            (QuotaTier::Total, Daily),
            (QuotaTier::Total, Monthly),
        ]
    );
}

#[test]
fn next_reset_is_present_only_when_the_entry_warns_or_is_exhausted() {
    let rows = vec![
        // total daily: 85% used -> warning, not exhausted
        BucketRow::new(Daily, Total, 85, 0),
        // total monthly: untouched
        // premium daily: exhausted
        BucketRow::new(Daily, TierPremium, 100, 0),
    ];

    let warnings = build_warnings(&limits(7, 100, 100, 100, 100), &rows, 80, keys());

    let by = |tier, period| -> QuotaWarningView {
        *warnings
            .iter()
            .find(|w| w.tier == tier && w.period == period)
            .unwrap()
    };
    let premium_daily = by(QuotaTier::Premium, Daily);
    assert!(premium_daily.exhausted && premium_daily.warning);
    assert_eq!(premium_daily.remaining_percentage, 0);
    assert_eq!(premium_daily.next_reset, Some(utc(2026, 3, 1, 0, 0, 0)));

    let total_daily = by(QuotaTier::Total, Daily);
    assert!(total_daily.warning && !total_daily.exhausted);
    assert_eq!(total_daily.remaining_percentage, 15);
    assert_eq!(total_daily.next_reset, Some(utc(2026, 3, 1, 0, 0, 0)));

    let untouched = by(QuotaTier::Total, Monthly);
    assert!(!untouched.warning && !untouched.exhausted);
    assert_eq!(untouched.remaining_percentage, 100);
    assert_eq!(untouched.next_reset, None);
    assert_eq!(by(QuotaTier::Premium, Monthly).next_reset, None);
}

#[test]
fn warnings_skip_periods_without_a_positive_limit() {
    let warnings = build_warnings(&limits(7, 0, 100, -1, 100), &[], 80, keys());

    assert_eq!(
        warnings
            .iter()
            .map(|w| (w.tier, w.period))
            .collect::<Vec<_>>(),
        vec![(QuotaTier::Premium, Monthly), (QuotaTier::Total, Monthly)]
    );
}

#[test]
fn warnings_use_the_same_arithmetic_as_the_status() {
    let rows = vec![BucketRow::new(Daily, Total, 80_000_001, 0)];
    let status = status(100_000_000, 80_000_001, 0, 80);

    let warnings = build_warnings(&total_limits(100_000_000, 100_000_000), &rows, 80, keys());

    let daily_total = warnings
        .iter()
        .find(|w| w.tier == QuotaTier::Total && w.period == Daily)
        .unwrap();
    assert_eq!(
        daily_total.remaining_percentage,
        status.remaining_percentage
    );
    assert_eq!(daily_total.warning, status.warning);
    assert_eq!(daily_total.exhausted, status.exhausted);
}
