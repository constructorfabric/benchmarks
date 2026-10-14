//! Tier availability tests: the bucket and limit mapping, the overflow-safe comparison (widened
//! arithmetic, no wrap, no panic), all-periods-and-buckets-must-pass, the DESIGN 5.10 numbers
//! and the explicit unavailability of a candidate whose credit computation failed.
//!
//! Slice S1-all: algorithms `cpt-cf-mini-chat-algo-quota-and-billing-tier-availability` and
//! `cpt-cf-mini-chat-algo-quota-and-billing-policy-binding` (limit mapping), definition of done
//! `cpt-cf-mini-chat-dod-quota-and-billing-preflight-downgrade`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mini_chat_sdk::ModelTier;

use super::{
    Bucket, BucketLimit, bucket_available, checked_limits, limit_of, required_buckets,
    tier_available,
};
use crate::domain::quota::periods::PeriodType;
use crate::domain::quota::rows::{BucketRow, QuotaRows};
use crate::test_support::USER_A1;
use crate::test_support::quota::{design_limits, limits};

fn row(spent: i64, reserved: i64) -> BucketRow {
    BucketRow {
        spent_credits_micro: spent,
        reserved_credits_micro: reserved,
        ..BucketRow::default()
    }
}

/// The DESIGN 5.10 consumption: premium 20M/200M, total 25M/240M (spent).
fn design_rows() -> QuotaRows {
    QuotaRows::new()
        .with(PeriodType::Daily, Bucket::TierPremium, row(20_000_000, 0))
        .with(
            PeriodType::Monthly,
            Bucket::TierPremium,
            row(200_000_000, 0),
        )
        .with(PeriodType::Daily, Bucket::Total, row(25_000_000, 0))
        .with(PeriodType::Monthly, Bucket::Total, row(240_000_000, 0))
}

// ── buckets and limits ───────────────────────────────────────────────────────

#[test]
fn the_standard_tier_requires_the_total_bucket_only() {
    assert_eq!(required_buckets(ModelTier::Standard), &[Bucket::Total]);
}

#[test]
fn the_premium_tier_requires_the_total_bucket_and_the_premium_sub_cap() {
    assert_eq!(
        required_buckets(ModelTier::Premium),
        &[Bucket::Total, Bucket::TierPremium]
    );
}

#[test]
fn the_bucket_names_are_the_row_values() {
    assert_eq!(Bucket::Total.as_str(), "total");
    assert_eq!(Bucket::TierPremium.as_str(), "tier:premium");
}

#[test]
fn the_total_bucket_takes_the_standard_limits_and_the_premium_bucket_the_premium_limits() {
    let user = design_limits(USER_A1);
    assert_eq!(
        limit_of(&user, Bucket::Total, PeriodType::Daily),
        60_000_000
    );
    assert_eq!(
        limit_of(&user, Bucket::Total, PeriodType::Monthly),
        600_000_000
    );
    assert_eq!(
        limit_of(&user, Bucket::TierPremium, PeriodType::Daily),
        22_000_000
    );
    assert_eq!(
        limit_of(&user, Bucket::TierPremium, PeriodType::Monthly),
        300_000_000
    );
}

#[test]
fn a_limit_above_i64_max_saturates() {
    let user = limits(USER_A1, 1, (u64::MAX, u64::MAX), (u64::MAX, 0));
    assert_eq!(limit_of(&user, Bucket::Total, PeriodType::Daily), i64::MAX);
    assert_eq!(
        limit_of(&user, Bucket::TierPremium, PeriodType::Daily),
        i64::MAX
    );
    assert_eq!(limit_of(&user, Bucket::TierPremium, PeriodType::Monthly), 0);
}

#[test]
fn the_checked_limits_of_a_standard_tier_are_the_total_bucket_of_both_periods() {
    assert_eq!(
        checked_limits(ModelTier::Standard, &design_limits(USER_A1)),
        vec![
            BucketLimit {
                period: PeriodType::Daily,
                bucket: Bucket::Total,
                limit_credits_micro: 60_000_000
            },
            BucketLimit {
                period: PeriodType::Monthly,
                bucket: Bucket::Total,
                limit_credits_micro: 600_000_000
            },
        ]
    );
}

#[test]
fn the_checked_limits_of_a_premium_tier_cover_both_buckets_and_both_periods() {
    let checked = checked_limits(ModelTier::Premium, &design_limits(USER_A1));
    assert_eq!(checked.len(), 4);
    for (bucket, period, limit) in [
        (Bucket::Total, PeriodType::Daily, 60_000_000),
        (Bucket::Total, PeriodType::Monthly, 600_000_000),
        (Bucket::TierPremium, PeriodType::Daily, 22_000_000),
        (Bucket::TierPremium, PeriodType::Monthly, 300_000_000),
    ] {
        assert!(
            checked.contains(&BucketLimit {
                period,
                bucket,
                limit_credits_micro: limit
            }),
            "{bucket:?} {period:?}: {checked:?}"
        );
    }
}

// ── the comparison ───────────────────────────────────────────────────────────

#[test]
fn a_candidate_that_fits_exactly_is_available() {
    // spent + reserved + candidate == limit
    assert!(bucket_available(&row(600, 300), 100, 1000));
    assert!(!bucket_available(&row(600, 300), 101, 1000));
}

#[test]
fn in_flight_reserves_count_against_the_limit() {
    assert!(bucket_available(&row(500, 0), 500, 1000));
    assert!(!bucket_available(&row(500, 1), 500, 1000));
}

#[test]
fn a_missing_row_counts_as_zero_spent_and_zero_reserved() {
    assert!(bucket_available(&BucketRow::default(), 1000, 1000));
    assert!(!bucket_available(&BucketRow::default(), 1001, 1000));
}

#[test]
fn a_negative_remainder_fails_the_check() {
    // already over the limit
    assert!(!bucket_available(&row(1200, 0), 1, 1000));
    assert!(!bucket_available(&row(800, 300), 1, 1000));
}

#[test]
fn a_zero_limit_admits_nothing() {
    assert!(!bucket_available(&BucketRow::default(), 1, 0));
}

#[test]
fn the_condition_is_the_same_as_spent_plus_reserved_plus_candidate_within_the_limit() {
    for spent in [0_i64, 1, 499, 500] {
        for reserved in [0_i64, 1, 250] {
            for candidate in [0_i64, 1, 250, 500, 1000] {
                for limit in [0_i64, 500, 1000] {
                    let expected = spent + reserved + candidate <= limit;
                    assert_eq!(
                        bucket_available(&row(spent, reserved), candidate, limit),
                        expected,
                        "spent {spent} reserved {reserved} candidate {candidate} limit {limit}"
                    );
                }
            }
        }
    }
}

#[test]
fn a_huge_candidate_on_a_bucket_with_spent_credits_neither_wraps_nor_panics() {
    // spent + reserved + i64::MAX would wrap a 64-bit sum
    assert!(!bucket_available(&row(10, 20), i64::MAX, i64::MAX));
    assert!(!bucket_available(
        &row(i64::MAX - 5, 10),
        i64::MAX,
        i64::MAX
    ));
    assert!(!bucket_available(
        &row(i64::MAX, i64::MAX),
        i64::MAX,
        i64::MAX
    ));
    assert!(!bucket_available(&row(i64::MAX, 0), 1, i64::MAX));
}

#[test]
fn counters_near_the_maximum_are_compared_in_widened_arithmetic() {
    // spent + reserved overflows i64 but the remainder is simply negative
    assert!(!bucket_available(&row(i64::MAX - 1, 10), 0, i64::MAX));
    // an empty bucket admits a candidate up to the limit itself
    assert!(bucket_available(&BucketRow::default(), i64::MAX, i64::MAX));
}

// ── a tier ───────────────────────────────────────────────────────────────────

#[test]
fn the_premium_candidate_of_the_design_example_fails_on_the_premium_daily_sub_cap() {
    // 20_000_000 + 3_750_000 > 22_000_000 although the total bucket passes
    assert!(!tier_available(
        ModelTier::Premium,
        3_750_000,
        false,
        &design_limits(USER_A1),
        &design_rows()
    ));
}

#[test]
fn the_standard_candidate_of_the_design_example_passes() {
    assert!(tier_available(
        ModelTier::Standard,
        1_500_000,
        false,
        &design_limits(USER_A1),
        &design_rows()
    ));
}

#[test]
fn an_empty_usage_makes_both_tiers_available_when_the_reserve_fits() {
    let rows = QuotaRows::new();
    assert!(tier_available(
        ModelTier::Premium,
        3_750_000,
        false,
        &design_limits(USER_A1),
        &rows
    ));
    assert!(tier_available(
        ModelTier::Standard,
        1_500_000,
        false,
        &design_limits(USER_A1),
        &rows
    ));
}

#[test]
fn a_pass_in_the_daily_period_and_a_failure_in_the_monthly_period_is_unavailable() {
    let rows = QuotaRows::new().with(PeriodType::Monthly, Bucket::Total, row(599_000_000, 0));
    assert!(!tier_available(
        ModelTier::Standard,
        2_000_000,
        false,
        &design_limits(USER_A1),
        &rows
    ));
    // the same candidate with room left in the monthly period passes
    let roomy = QuotaRows::new().with(PeriodType::Monthly, Bucket::Total, row(590_000_000, 0));
    assert!(tier_available(
        ModelTier::Standard,
        2_000_000,
        false,
        &design_limits(USER_A1),
        &roomy
    ));
}

#[test]
fn a_pass_in_the_monthly_period_and_a_failure_in_the_daily_period_is_unavailable() {
    let rows = QuotaRows::new().with(PeriodType::Daily, Bucket::Total, row(59_000_000, 0));
    assert!(!tier_available(
        ModelTier::Standard,
        2_000_000,
        false,
        &design_limits(USER_A1),
        &rows
    ));
}

#[test]
fn the_standard_tier_ignores_an_exhausted_premium_sub_cap() {
    let rows = QuotaRows::new().with(PeriodType::Daily, Bucket::TierPremium, row(22_000_000, 0));
    assert!(tier_available(
        ModelTier::Standard,
        1_500_000,
        false,
        &design_limits(USER_A1),
        &rows
    ));
}

#[test]
fn the_premium_tier_is_unavailable_when_only_the_total_bucket_is_exhausted() {
    let rows = QuotaRows::new().with(PeriodType::Daily, Bucket::Total, row(59_000_000, 0));
    assert!(!tier_available(
        ModelTier::Premium,
        3_750_000,
        false,
        &design_limits(USER_A1),
        &rows
    ));
}

#[test]
fn the_premium_monthly_sub_cap_alone_makes_the_premium_tier_unavailable() {
    let rows = QuotaRows::new().with(
        PeriodType::Monthly,
        Bucket::TierPremium,
        row(299_000_000, 0),
    );
    assert!(!tier_available(
        ModelTier::Premium,
        3_750_000,
        false,
        &design_limits(USER_A1),
        &rows
    ));
}

#[test]
fn an_in_flight_reserve_in_any_required_row_counts() {
    let rows = QuotaRows::new().with(PeriodType::Daily, Bucket::Total, row(0, 59_000_000));
    assert!(!tier_available(
        ModelTier::Standard,
        2_000_000,
        false,
        &design_limits(USER_A1),
        &rows
    ));
}

#[test]
fn a_zero_limit_makes_the_tier_unavailable() {
    let no_premium = limits(USER_A1, 1, (60_000_000, 600_000_000), (0, 0));
    assert!(!tier_available(
        ModelTier::Premium,
        1,
        false,
        &no_premium,
        &QuotaRows::new()
    ));
    assert!(tier_available(
        ModelTier::Standard,
        1,
        false,
        &no_premium,
        &QuotaRows::new()
    ));
}

// ── a candidate whose credit computation failed ──────────────────────────────

#[test]
fn a_failed_candidate_is_unavailable_whatever_the_rows_contain() {
    let max = i64::MAX.unsigned_abs();
    let user = limits(USER_A1, 1, (max, max), (max, max));
    // empty rows and a limit equal to the i64::MAX sentinel: the sentinel would "fit"
    for tier in [ModelTier::Standard, ModelTier::Premium] {
        assert!(!tier_available(
            tier,
            i64::MAX,
            true,
            &user,
            &QuotaRows::new()
        ));
    }
}

#[test]
fn a_failed_candidate_on_buckets_with_spent_and_reserved_credits_does_not_wrap_or_panic() {
    let rows = QuotaRows::new()
        .with(PeriodType::Daily, Bucket::Total, row(i64::MAX - 1, 1_000))
        .with(PeriodType::Monthly, Bucket::Total, row(5, 7))
        .with(PeriodType::Daily, Bucket::TierPremium, row(3, 3))
        .with(PeriodType::Monthly, Bucket::TierPremium, row(1, 1));
    for tier in [ModelTier::Standard, ModelTier::Premium] {
        assert!(!tier_available(
            tier,
            i64::MAX,
            true,
            &design_limits(USER_A1),
            &rows
        ));
    }
}

#[test]
fn a_large_computable_candidate_on_non_zero_counters_is_simply_unavailable() {
    let rows = QuotaRows::new().with(PeriodType::Daily, Bucket::Total, row(10, 20));
    assert!(!tier_available(
        ModelTier::Standard,
        i64::MAX - 1,
        false,
        &design_limits(USER_A1),
        &rows
    ));
}
