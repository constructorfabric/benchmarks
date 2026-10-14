//! Quota status computation (`cpt-cf-mini-chat-algo-quota-and-billing-status-computation`) and
//! the quota warnings projection (`cpt-cf-mini-chat-algo-quota-and-billing-quota-warnings`):
//! one computation over the same rows serves `GET /v1/quota/status` and the `quota_warnings`
//! of the SSE `done` event.

use mini_chat_sdk::UserLimits;
use time::OffsetDateTime;
use toolkit_macros::domain_model;

use crate::domain::quota::availability::limit_of;
use crate::domain::quota::periods::{PeriodType, next_reset};
use crate::domain::quota::rows::{Bucket, QuotaRows};

/// The tier of a status entry: `premium` reads bucket `tier:premium` with the premium limits,
/// `total` reads bucket `total` with the standard limits.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaTier {
    Premium,
    Total,
}

impl QuotaTier {
    /// The wire value: `premium` or `total`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Premium => "premium",
            Self::Total => "total",
        }
    }
}

/// One period of one tier.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeriodStatus {
    pub period: PeriodType,
    pub limit_credits_micro: i64,
    /// `spent_credits_micro + reserved_credits_micro` (in-flight reserves count as used).
    pub used_credits_micro: i64,
    /// The limit minus the used credits, not below 0.
    pub remaining_credits_micro: i64,
    /// `remaining * 100 / limit` as a floored integer from 0 to 100.
    pub remaining_percentage: u8,
    pub next_reset: OffsetDateTime,
    pub warning: bool,
    pub exhausted: bool,
}

/// The periods of one tier, in the order daily, monthly.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierStatus {
    pub tier: QuotaTier,
    pub periods: Vec<PeriodStatus>,
}

/// The status of a user: the tiers in the order premium, total, and the configured threshold.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaStatus {
    pub tiers: Vec<TierStatus>,
    pub warning_threshold_pct: u8,
}

/// An entry of `quota_warnings` of the `done` event.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaWarning {
    pub tier: QuotaTier,
    pub period: PeriodType,
    pub remaining_percentage: u8,
    pub warning: bool,
    pub exhausted: bool,
    /// Present only when `warning` or `exhausted` is true.
    pub next_reset: Option<OffsetDateTime>,
}

/// Compute the tiers: skip a period whose limit is 0 or negative, omit a tier without a listed
/// period, `warning` when `remaining_percentage <= 100 - warning_threshold_pct`, `exhausted`
/// when `remaining_percentage == 0` (already true below 1 percent remaining). Integer
/// arithmetic wide enough not to overflow. `now` selects the periods of `next_reset`.
#[must_use]
pub fn compute_status(
    limits: &UserLimits,
    rows: &QuotaRows,
    warning_threshold_pct: u8,
    now: OffsetDateTime,
) -> Vec<TierStatus> {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-status-computation:p1:inst-qs-for-each-tier
    // the tiers in the order `premium`, `total`: `premium` reads bucket `tier:premium` with the
    // premium limits, `total` reads bucket `total` with the standard limits
    let tiers = [
        (QuotaTier::Premium, Bucket::TierPremium),
        (QuotaTier::Total, Bucket::Total),
    ];
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-status-computation:p1:inst-qs-omit-empty-tier
    // a tier without a listed period is omitted: no period with a positive limit gives an
    // empty array; tool quotas, billing outcomes and settlement details are not part of it
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-status-computation:p1:inst-qs-exclusions
    let listed = tiers
        .into_iter()
        .map(|(tier, bucket)| TierStatus {
            tier,
            periods: tier_periods(limits, rows, bucket, warning_threshold_pct, now),
        })
        .filter(|tier| !tier.periods.is_empty())
        .collect();
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-status-computation:p1:inst-qs-exclusions
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-status-computation:p1:inst-qs-omit-empty-tier
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-status-computation:p1:inst-qs-for-each-tier
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-status-computation:p1:inst-qs-return
    listed
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-status-computation:p1:inst-qs-return
}

/// The listed periods of one bucket, in the order daily, monthly.
fn tier_periods(
    limits: &UserLimits,
    rows: &QuotaRows,
    bucket: Bucket,
    warning_threshold_pct: u8,
    now: OffsetDateTime,
) -> Vec<PeriodStatus> {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-status-computation:p1:inst-qs-for-each-period
    // the periods in the order `daily`, `monthly`
    let periods = PeriodType::ALL
        .into_iter()
        .filter_map(|period| {
            let limit = limit_of(limits, bucket, period);
            // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-status-computation:p1:inst-qs-skip-no-limit
            // a period whose limit is 0 or negative appears neither in the status nor in the
            // warnings
            if limit <= 0 {
                return None;
            }
            // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-status-computation:p1:inst-qs-skip-no-limit
            Some(period_status(
                period,
                limit,
                rows,
                bucket,
                warning_threshold_pct,
                now,
            ))
        })
        .collect::<Vec<_>>();
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-status-computation:p1:inst-qs-for-each-period
    periods
}

/// One period with a positive limit: integer arithmetic wide enough not to overflow.
fn period_status(
    period: PeriodType,
    limit: i64,
    rows: &QuotaRows,
    bucket: Bucket,
    warning_threshold_pct: u8,
    now: OffsetDateTime,
) -> PeriodStatus {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-status-computation:p1:inst-qs-row
    // a missing row is all zeros
    let row = rows.get(period, bucket);
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-status-computation:p1:inst-qs-row
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-status-computation:p1:inst-qs-used
    // conservative: in-flight reserves count as used
    let used = row
        .spent_credits_micro
        .saturating_add(row.reserved_credits_micro)
        .max(0);
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-status-computation:p1:inst-qs-used
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-status-computation:p1:inst-qs-remaining
    let remaining = limit.saturating_sub(used).max(0);
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-status-computation:p1:inst-qs-remaining
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-status-computation:p1:inst-qs-percentage
    // `remaining * 100 / limit` floored, in 128-bit arithmetic
    let percentage = (i128::from(remaining) * 100)
        .checked_div(i128::from(limit))
        .and_then(|value| u8::try_from(value).ok())
        .unwrap_or(0)
        .min(100);
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-status-computation:p1:inst-qs-percentage
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-status-computation:p1:inst-qs-flags
    // `exhausted` already holds when less than 1 percent of the limit remains
    let warning = percentage <= 100_u8.saturating_sub(warning_threshold_pct);
    let exhausted = percentage == 0;
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-status-computation:p1:inst-qs-flags
    PeriodStatus {
        period,
        limit_credits_micro: limit,
        used_credits_micro: used,
        remaining_credits_micro: remaining,
        remaining_percentage: percentage,
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-status-computation:p1:inst-qs-next-reset
        next_reset: next_reset(period, now),
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-status-computation:p1:inst-qs-next-reset
        warning,
        exhausted,
    }
}

/// One entry per listed tier and period, with `next_reset` only when `warning` or `exhausted`.
#[must_use]
pub fn quota_warnings(tiers: &[TierStatus]) -> Vec<QuotaWarning> {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-quota-warnings:p1:inst-qw-same-computation
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-quota-warnings:p1:inst-qw-next-reset
    // one entry per listed tier and period, from the same computation as the status, so the SSE
    // values and the status values agree for the same rows; `next_reset` only when the entry
    // warns or is exhausted. The caller reads the rows after the turn's settlement and decides
    // whether to attach the entries to the `done` event.
    let entries = tiers
        .iter()
        .flat_map(|tier| {
            tier.periods.iter().map(move |period| QuotaWarning {
                tier: tier.tier,
                period: period.period,
                remaining_percentage: period.remaining_percentage,
                warning: period.warning,
                exhausted: period.exhausted,
                next_reset: (period.warning || period.exhausted).then_some(period.next_reset),
            })
        })
        .collect();
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-quota-warnings:p1:inst-qw-next-reset
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-quota-warnings:p1:inst-qw-same-computation
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-quota-warnings:p1:inst-qw-return
    entries
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-quota-warnings:p1:inst-qw-return
}

#[cfg(test)]
#[path = "status_tests.rs"]
mod status_tests;
