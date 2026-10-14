//! Tier availability (`cpt-cf-mini-chat-algo-quota-and-billing-tier-availability`): the
//! overflow-safe `bucket_available` check of DESIGN 5.4.2 and the bucket and limit mapping of
//! the policy binding (`cpt-cf-mini-chat-algo-quota-and-billing-policy-binding`).

use mini_chat_sdk::{ModelTier, UserLimits};
use toolkit_macros::domain_model;

use crate::domain::quota::periods::PeriodType;
use crate::domain::quota::rows::{Bucket, BucketRow, QuotaRows};

/// The user limit of one bucket and period, as checked by the decision.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BucketLimit {
    pub period: PeriodType,
    pub bucket: Bucket,
    pub limit_credits_micro: i64,
}

/// The buckets a tier needs: standard needs `total`; premium needs `total` and `tier:premium`
/// (in that order).
#[must_use]
pub fn required_buckets(tier: ModelTier) -> &'static [Bucket] {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-tier-availability:p1:inst-avail-buckets
    match tier {
        ModelTier::Standard => &[Bucket::Total],
        ModelTier::Premium => &[Bucket::Total, Bucket::TierPremium],
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-tier-availability:p1:inst-avail-buckets
}

/// The limit of a bucket and period: `total` takes `UserLimits::standard` (the global ceiling,
/// not a standard-only cap), `tier:premium` takes `UserLimits::premium`; a limit above
/// `i64::MAX` saturates to `i64::MAX`.
#[must_use]
pub fn limit_of(limits: &UserLimits, bucket: Bucket, period: PeriodType) -> i64 {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-policy-binding:p1:inst-policy-limit-mapping
    // `total` is the global ceiling over all tiers (the standard limits); `tier:premium` is
    // the premium sub-cap (the premium limits)
    let tier = match bucket {
        Bucket::Total => &limits.standard,
        Bucket::TierPremium => &limits.premium,
    };
    let limit = match period {
        PeriodType::Daily => tier.limit_daily_credits_micro,
        PeriodType::Monthly => tier.limit_monthly_credits_micro,
    };
    i64::try_from(limit).unwrap_or(i64::MAX)
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-policy-binding:p1:inst-policy-limit-mapping
}

/// The limits of every required bucket of `tier` in both periods (what the reserve write
/// re-checks): the buckets in [`required_buckets`] order, each for daily then monthly.
#[must_use]
pub fn checked_limits(tier: ModelTier, limits: &UserLimits) -> Vec<BucketLimit> {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-tier-availability:p1:inst-avail-periods
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-tier-availability:p1:inst-avail-for-each
    required_buckets(tier)
        .iter()
        .flat_map(|&bucket| {
            PeriodType::ALL.into_iter().map(move |period| BucketLimit {
                period,
                bucket,
                limit_credits_micro: limit_of(limits, bucket, period),
            })
        })
        .collect()
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-tier-availability:p1:inst-avail-for-each
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-tier-availability:p1:inst-avail-periods
}

/// `candidate <= limit - spent - reserved`, evaluated in 128-bit signed arithmetic: the same
/// condition as `spent + reserved + candidate <= limit` without ever computing the 64-bit sum,
/// so a large candidate or large counters can neither wrap nor panic, and a negative remainder
/// fails the check.
#[must_use]
pub fn bucket_available(
    row: &BucketRow,
    candidate_credits_micro: i64,
    limit_credits_micro: i64,
) -> bool {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-tier-availability:p1:inst-avail-compare
    // `candidate <= limit - spent - reserved` in 128-bit signed arithmetic: the 64-bit sum is
    // never computed, so nothing wraps, and a negative remainder fails the check
    let remainder = i128::from(limit_credits_micro)
        - i128::from(row.spent_credits_micro)
        - i128::from(row.reserved_credits_micro);
    i128::from(candidate_credits_micro) <= remainder
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-tier-availability:p1:inst-avail-compare
}

/// Whether every required bucket passes in both periods. A candidate whose credit computation
/// failed is unavailable at once, whatever the rows contain.
#[must_use]
pub fn tier_available(
    tier: ModelTier,
    candidate_credits_micro: i64,
    credit_computation_failed: bool,
    limits: &UserLimits,
    rows: &QuotaRows,
) -> bool {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-tier-availability:p1:inst-avail-failed-candidate
    // a candidate whose credit computation failed is unavailable at once, before any
    // comparison and whatever the rows contain; the `i64::MAX` sentinel is never booked
    if credit_computation_failed {
        return false;
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-tier-availability:p1:inst-avail-failed-candidate
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-tier-availability:p1:inst-avail-all-must-pass
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-tier-availability:p1:inst-avail-no-threshold
    // the rows were read by the caller in one transaction (a missing row is all zeros); one
    // exhausted period or bucket makes the whole tier unavailable, and no negative-threshold
    // knob exists
    checked_limits(tier, limits).iter().all(|limit| {
        bucket_available(
            &rows.get(limit.period, limit.bucket),
            candidate_credits_micro,
            limit.limit_credits_micro,
        )
    })
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-tier-availability:p1:inst-avail-no-threshold
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-tier-availability:p1:inst-avail-all-must-pass
}

#[cfg(test)]
#[path = "availability_tests.rs"]
mod availability_tests;
