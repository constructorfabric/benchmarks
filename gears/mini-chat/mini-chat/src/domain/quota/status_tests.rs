//! Quota status computation and warnings projection tests: tier and period order, used as
//! spent plus reserved, floored percentages, the warning and exhausted flags at the threshold
//! boundaries, clamping, skipped limits, wide arithmetic, `next_reset` and the warnings that
//! agree with the status.
//!
//! Slice S1-all: algorithms `cpt-cf-mini-chat-algo-quota-and-billing-status-computation` and
//! `cpt-cf-mini-chat-algo-quota-and-billing-quota-warnings`, definition of done
//! `cpt-cf-mini-chat-dod-quota-and-billing-warning-thresholds`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mini_chat_sdk::UserLimits;

use super::{PeriodStatus, QuotaTier, QuotaWarning, TierStatus, compute_status, quota_warnings};
use crate::domain::quota::periods::PeriodType;
use crate::domain::quota::rows::{Bucket, BucketRow, QuotaRows};
use crate::test_support::USER_A1;
use crate::test_support::quota::{default_now, design_limits, limits, utc};

fn rows(spent: i64, reserved: i64) -> BucketRow {
    BucketRow {
        spent_credits_micro: spent,
        reserved_credits_micro: reserved,
        ..BucketRow::default()
    }
}

/// Limits of 1000 credits-micro in every bucket and period.
fn thousand() -> UserLimits {
    limits(USER_A1, 1, (1000, 1000), (1000, 1000))
}

fn status(user: &UserLimits, usage: &QuotaRows, threshold: u8) -> Vec<TierStatus> {
    compute_status(user, usage, threshold, default_now())
}

fn period(tiers: &[TierStatus], tier: QuotaTier, period: PeriodType) -> PeriodStatus {
    tiers
        .iter()
        .find(|t| t.tier == tier)
        .unwrap_or_else(|| panic!("tier {tier:?} missing in {tiers:?}"))
        .periods
        .iter()
        .find(|p| p.period == period)
        .copied()
        .unwrap_or_else(|| panic!("period {period:?} missing in {tiers:?}"))
}

/// Total daily usage of `used` against the limit of 1000.
fn daily_total(used: i64, threshold: u8) -> PeriodStatus {
    let usage = QuotaRows::new().with(PeriodType::Daily, Bucket::Total, rows(used, 0));
    period(
        &status(&thousand(), &usage, threshold),
        QuotaTier::Total,
        PeriodType::Daily,
    )
}

// ── shape and order ──────────────────────────────────────────────────────────

#[test]
fn the_tiers_are_premium_then_total_and_the_periods_daily_then_monthly() {
    let tiers = status(&design_limits(USER_A1), &QuotaRows::new(), 80);
    assert_eq!(
        tiers.iter().map(|t| t.tier).collect::<Vec<_>>(),
        vec![QuotaTier::Premium, QuotaTier::Total]
    );
    for tier in &tiers {
        assert_eq!(
            tier.periods.iter().map(|p| p.period).collect::<Vec<_>>(),
            vec![PeriodType::Daily, PeriodType::Monthly]
        );
    }
}

#[test]
fn the_tier_names_are_the_wire_values() {
    assert_eq!(QuotaTier::Premium.as_str(), "premium");
    assert_eq!(QuotaTier::Total.as_str(), "total");
}

#[test]
fn a_user_without_usage_rows_has_everything_remaining() {
    let tiers = status(&design_limits(USER_A1), &QuotaRows::new(), 80);
    for tier in &tiers {
        for entry in &tier.periods {
            assert_eq!(entry.used_credits_micro, 0, "{tier:?}");
            assert_eq!(entry.remaining_credits_micro, entry.limit_credits_micro);
            assert_eq!(entry.remaining_percentage, 100);
            assert!(!entry.warning);
            assert!(!entry.exhausted);
        }
    }
    assert_eq!(
        period(&tiers, QuotaTier::Premium, PeriodType::Daily).limit_credits_micro,
        22_000_000
    );
    assert_eq!(
        period(&tiers, QuotaTier::Total, PeriodType::Monthly).limit_credits_micro,
        600_000_000
    );
}

#[test]
fn each_tier_reads_its_own_bucket_and_limits() {
    // premium: 11M of 22M used; total: 6M of 60M used
    let usage = QuotaRows::new()
        .with(PeriodType::Daily, Bucket::TierPremium, rows(11_000_000, 0))
        .with(PeriodType::Daily, Bucket::Total, rows(6_000_000, 0));
    let tiers = status(&design_limits(USER_A1), &usage, 80);
    let premium = period(&tiers, QuotaTier::Premium, PeriodType::Daily);
    assert_eq!(premium.limit_credits_micro, 22_000_000);
    assert_eq!(premium.used_credits_micro, 11_000_000);
    assert_eq!(premium.remaining_percentage, 50);
    let total = period(&tiers, QuotaTier::Total, PeriodType::Daily);
    assert_eq!(total.limit_credits_micro, 60_000_000);
    assert_eq!(total.used_credits_micro, 6_000_000);
    assert_eq!(total.remaining_percentage, 90);
}

// ── used, remaining and the percentage ───────────────────────────────────────

#[test]
fn used_is_spent_plus_reserved_and_remaining_is_the_rest() {
    let usage = QuotaRows::new().with(
        PeriodType::Daily,
        Bucket::Total,
        rows(25_000_000, 3_000_000),
    );
    let entry = period(
        &status(&design_limits(USER_A1), &usage, 80),
        QuotaTier::Total,
        PeriodType::Daily,
    );
    assert_eq!(entry.used_credits_micro, 28_000_000);
    assert_eq!(entry.remaining_credits_micro, 32_000_000);
    // 32 / 60 is 53.33 percent: floored
    assert_eq!(entry.remaining_percentage, 53);
}

#[test]
fn the_percentage_is_floored() {
    assert_eq!(
        daily_total(1, 80).remaining_percentage,
        99,
        "999 / 1000 is 99.9 percent"
    );
    assert_eq!(
        daily_total(333, 80).remaining_percentage,
        66,
        "667 / 1000 is 66.7 percent"
    );
    assert_eq!(daily_total(500, 80).remaining_percentage, 50);
    assert_eq!(daily_total(0, 80).remaining_percentage, 100);
}

#[test]
fn less_than_one_percent_remaining_is_zero_percent_and_exhausted() {
    let entry = daily_total(995, 80);
    assert_eq!(entry.remaining_credits_micro, 5);
    assert_eq!(entry.remaining_percentage, 0);
    assert!(entry.exhausted);
    assert!(entry.warning);
}

#[test]
fn a_fully_used_limit_is_exhausted() {
    let entry = daily_total(1000, 80);
    assert_eq!(entry.remaining_credits_micro, 0);
    assert_eq!(entry.remaining_percentage, 0);
    assert!(entry.exhausted);
}

#[test]
fn usage_above_the_limit_clamps_the_remainder_at_zero() {
    let entry = daily_total(1500, 80);
    assert_eq!(entry.used_credits_micro, 1500);
    assert_eq!(entry.remaining_credits_micro, 0);
    assert_eq!(entry.remaining_percentage, 0);
    assert!(entry.exhausted);
}

// ── warning thresholds ───────────────────────────────────────────────────────

#[test]
fn with_the_threshold_80_twenty_percent_remaining_warns_and_twenty_one_does_not() {
    let at_boundary = daily_total(800, 80);
    assert_eq!(at_boundary.remaining_percentage, 20);
    assert!(at_boundary.warning);
    assert!(!at_boundary.exhausted);

    let above = daily_total(790, 80);
    assert_eq!(above.remaining_percentage, 21);
    assert!(!above.warning);
}

#[test]
fn exhausted_holds_only_at_zero_percent() {
    assert!(!daily_total(990, 80).exhausted, "1 percent remains");
    assert!(daily_total(991, 80).exhausted, "0.9 percent floors to 0");
}

#[test]
fn the_threshold_range_ends_are_honoured() {
    // 99: warn when at most 1 percent remains
    assert!(daily_total(990, 99).warning);
    assert!(!daily_total(980, 99).warning);
    // 1: warn when at most 99 percent remains
    assert!(daily_total(10, 1).warning);
    assert!(!daily_total(0, 1).warning);
}

#[test]
fn a_custom_threshold_moves_the_warning_boundary() {
    assert!(daily_total(500, 50).warning);
    // 511 remaining of 1000 is 51 percent
    assert!(!daily_total(489, 50).warning);
}

// ── skipped limits ───────────────────────────────────────────────────────────

#[test]
fn a_period_with_a_zero_limit_is_omitted() {
    let user = limits(USER_A1, 1, (60_000_000, 600_000_000), (0, 300_000_000));
    let tiers = status(&user, &QuotaRows::new(), 80);
    let premium = tiers.iter().find(|t| t.tier == QuotaTier::Premium).unwrap();
    assert_eq!(
        premium.periods.iter().map(|p| p.period).collect::<Vec<_>>(),
        vec![PeriodType::Monthly]
    );
    let total = tiers.iter().find(|t| t.tier == QuotaTier::Total).unwrap();
    assert_eq!(total.periods.len(), 2);
}

#[test]
fn a_tier_without_a_listed_period_is_omitted() {
    let user = limits(USER_A1, 1, (60_000_000, 600_000_000), (0, 0));
    let tiers = status(&user, &QuotaRows::new(), 80);
    assert_eq!(
        tiers.iter().map(|t| t.tier).collect::<Vec<_>>(),
        vec![QuotaTier::Total]
    );
}

#[test]
fn limits_that_are_all_zero_give_an_empty_status() {
    let user = limits(USER_A1, 1, (0, 0), (0, 0));
    assert!(status(&user, &QuotaRows::new(), 80).is_empty());
    assert!(quota_warnings(&status(&user, &QuotaRows::new(), 80)).is_empty());
}

// ── wide arithmetic ──────────────────────────────────────────────────────────

#[test]
fn counters_near_the_maximum_neither_wrap_nor_panic() {
    let max = i64::MAX.unsigned_abs();
    let user = limits(USER_A1, 1, (max, max), (max, max));
    let usage = QuotaRows::new().with(PeriodType::Daily, Bucket::Total, rows(i64::MAX, i64::MAX));
    let entry = period(
        &status(&user, &usage, 80),
        QuotaTier::Total,
        PeriodType::Daily,
    );
    assert_eq!(entry.remaining_credits_micro, 0);
    assert_eq!(entry.remaining_percentage, 0);
    assert!(entry.exhausted);
    assert!(
        entry.used_credits_micro >= 0,
        "the used credits saturate instead of wrapping"
    );
}

#[test]
fn a_huge_limit_with_no_usage_is_one_hundred_percent() {
    let user = limits(USER_A1, 1, (u64::MAX, u64::MAX), (u64::MAX, u64::MAX));
    let entry = period(
        &status(&user, &QuotaRows::new(), 80),
        QuotaTier::Total,
        PeriodType::Daily,
    );
    assert_eq!(entry.limit_credits_micro, i64::MAX);
    assert_eq!(entry.remaining_credits_micro, i64::MAX);
    assert_eq!(entry.remaining_percentage, 100);
}

// ── next reset ───────────────────────────────────────────────────────────────

#[test]
fn next_reset_follows_the_period_boundaries_of_the_current_instant() {
    let tiers = compute_status(
        &design_limits(USER_A1),
        &QuotaRows::new(),
        80,
        utc(2026, 2, 15, 10, 0, 0),
    );
    for tier in [QuotaTier::Premium, QuotaTier::Total] {
        assert_eq!(
            period(&tiers, tier, PeriodType::Daily).next_reset,
            utc(2026, 2, 16, 0, 0, 0)
        );
        assert_eq!(
            period(&tiers, tier, PeriodType::Monthly).next_reset,
            utc(2026, 3, 1, 0, 0, 0)
        );
    }
}

// ── warnings projection ──────────────────────────────────────────────────────

fn warnings_of(user: &UserLimits, usage: &QuotaRows, threshold: u8) -> Vec<QuotaWarning> {
    quota_warnings(&status(user, usage, threshold))
}

#[test]
fn there_is_one_warning_entry_per_listed_tier_and_period_in_the_status_order() {
    let entries = warnings_of(&design_limits(USER_A1), &QuotaRows::new(), 80);
    assert_eq!(
        entries
            .iter()
            .map(|w| (w.tier, w.period))
            .collect::<Vec<_>>(),
        vec![
            (QuotaTier::Premium, PeriodType::Daily),
            (QuotaTier::Premium, PeriodType::Monthly),
            (QuotaTier::Total, PeriodType::Daily),
            (QuotaTier::Total, PeriodType::Monthly),
        ]
    );
}

#[test]
fn next_reset_is_present_only_when_the_entry_warns_or_is_exhausted() {
    let usage = QuotaRows::new()
        .with(PeriodType::Daily, Bucket::Total, rows(900, 0))
        .with(PeriodType::Monthly, Bucket::Total, rows(1000, 0))
        .with(PeriodType::Daily, Bucket::TierPremium, rows(100, 0));
    let entries = warnings_of(&thousand(), &usage, 80);
    let find = |tier: QuotaTier, period: PeriodType| {
        entries
            .iter()
            .find(|w| w.tier == tier && w.period == period)
            .copied()
            .unwrap()
    };

    let warns = find(QuotaTier::Total, PeriodType::Daily);
    assert!(warns.warning);
    assert!(!warns.exhausted);
    assert_eq!(warns.next_reset, Some(utc(2026, 3, 1, 0, 0, 0)));

    let exhausted = find(QuotaTier::Total, PeriodType::Monthly);
    assert!(exhausted.exhausted);
    assert!(exhausted.next_reset.is_some());

    let healthy = find(QuotaTier::Premium, PeriodType::Daily);
    assert!(!healthy.warning);
    assert!(!healthy.exhausted);
    assert_eq!(healthy.next_reset, None);
}

#[test]
fn the_warnings_agree_with_the_status_for_the_same_rows() {
    let usage = QuotaRows::new()
        .with(
            PeriodType::Daily,
            Bucket::TierPremium,
            rows(21_000_000, 500_000),
        )
        .with(
            PeriodType::Monthly,
            Bucket::Total,
            rows(500_000_000, 20_000_000),
        )
        .with(PeriodType::Daily, Bucket::Total, rows(30_000_000, 0));
    let tiers = status(&design_limits(USER_A1), &usage, 80);
    let entries = quota_warnings(&tiers);
    for tier in &tiers {
        for listed in &tier.periods {
            let entry = entries
                .iter()
                .find(|w| w.tier == tier.tier && w.period == listed.period)
                .unwrap();
            assert_eq!(entry.remaining_percentage, listed.remaining_percentage);
            assert_eq!(entry.warning, listed.warning);
            assert_eq!(entry.exhausted, listed.exhausted);
            if listed.warning || listed.exhausted {
                assert_eq!(entry.next_reset, Some(listed.next_reset));
            }
        }
    }
}

#[test]
fn a_period_with_a_zero_limit_is_absent_from_the_warnings() {
    let user = limits(USER_A1, 1, (1000, 1000), (0, 1000));
    let entries = warnings_of(&user, &QuotaRows::new(), 80);
    assert!(
        !entries
            .iter()
            .any(|w| w.tier == QuotaTier::Premium && w.period == PeriodType::Daily)
    );
    assert_eq!(entries.len(), 3);
}
