//! Quota view and warnings
//! (`cpt-cf-mini-chat-algo-quota-billing-quota-view`,
//! `cpt-cf-mini-chat-dod-quota-billing-quota-status`).

use mini_chat_sdk::UserLimits;
use time::OffsetDateTime;
use toolkit_macros::domain_model;

use super::periods::PeriodKeys;
use super::types::{BucketRow, QuotaPeriod, QuotaTier, find_row, limit_of};

/// One period of one tier of the status.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaPeriodView {
    /// `daily` or `monthly`.
    pub period: QuotaPeriod,
    /// The limit (always positive here).
    pub limit_credits_micro: i64,
    /// `spent + reserved` of the row (not clamped; 0 without a row).
    pub used_credits_micro: i64,
    /// `limit - used`, clamped at 0.
    pub remaining_credits_micro: i64,
    /// `remaining * 100 / limit`, floored, in 128-bit integers, within 0..=100.
    pub remaining_percentage: u8,
    /// The reset instant of the period.
    pub next_reset: OffsetDateTime,
    /// `remaining_percentage <= 100 - warning_threshold_pct`.
    pub warning: bool,
    /// `remaining_percentage == 0`.
    pub exhausted: bool,
}

/// One tier of the status.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaTierView {
    /// `premium` or `total`.
    pub tier: QuotaTier,
    /// Daily then monthly; a period whose limit is `<= 0` is omitted.
    pub periods: Vec<QuotaPeriodView>,
}

/// The status response of the domain: the tiers and the threshold.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaStatusView {
    /// `premium` then `total`; a tier whose periods are all omitted is still
    /// listed with no periods.
    pub tiers: Vec<QuotaTierView>,
    /// `quota.warning_threshold_pct`.
    pub warning_threshold_pct: u8,
}

/// One entry of `done.quota_warnings`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaWarningView {
    /// `premium` or `total`.
    pub tier: QuotaTier,
    /// `daily` or `monthly`.
    pub period: QuotaPeriod,
    /// Remaining percentage.
    pub remaining_percentage: u8,
    /// Warning flag.
    pub warning: bool,
    /// Exhausted flag.
    pub exhausted: bool,
    /// The reset instant; present only when `warning` or `exhausted` is true.
    pub next_reset: Option<OffsetDateTime>,
}

/// Status mode of the quota view: one entry per tier (`premium` with the
/// `tier:premium` bucket and `user_limits.premium`, then `total` with the
/// `total` bucket and `user_limits.standard`) and period (daily, monthly).
#[must_use]
pub fn build_status_view(
    limits: &UserLimits,
    rows: &[BucketRow],
    warning_threshold_pct: u8,
    keys: PeriodKeys,
) -> Vec<QuotaTierView> {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-quota-view:p1:inst-for-each-view-tier
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-quota-view:p1:inst-if-status-mode
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-quota-view:p1:inst-return-status-view
    QUOTA_TIERS
        .iter()
        .map(|tier| QuotaTierView {
            tier: *tier,
            // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-quota-view:p1:inst-for-each-view-period
            periods: QuotaPeriod::ALL
                .iter()
                .filter_map(|period| {
                    period_view(limits, rows, *tier, *period, warning_threshold_pct, keys)
                })
                .collect(),
            // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-quota-view:p1:inst-for-each-view-period
        })
        .collect()
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-quota-view:p1:inst-return-status-view
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-quota-view:p1:inst-if-status-mode
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-quota-view:p1:inst-for-each-view-tier
}

/// The tiers in the order of the status.
const QUOTA_TIERS: [QuotaTier; 2] = [QuotaTier::Premium, QuotaTier::Total];

/// The view of one tier and period; `None` when the limit is not positive.
fn period_view(
    limits: &UserLimits,
    rows: &[BucketRow],
    tier: QuotaTier,
    period: QuotaPeriod,
    warning_threshold_pct: u8,
    keys: PeriodKeys,
) -> Option<QuotaPeriodView> {
    let limit = limit_of(limits, tier.bucket(), period);
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-quota-view:p1:inst-skip-zero-limit
    if limit <= 0 {
        return None;
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-quota-view:p1:inst-skip-zero-limit
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-quota-view:p1:inst-view-used
    let used = find_row(rows, period, tier.bucket()).map_or(0, |row| {
        row.spent_credits_micro
            .saturating_add(row.reserved_credits_micro)
    });
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-quota-view:p1:inst-view-used
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-quota-view:p1:inst-view-remaining
    let remaining = limit.saturating_sub(used).max(0);
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-quota-view:p1:inst-view-remaining
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-quota-view:p1:inst-view-percentage
    let percentage = i128::from(remaining)
        .saturating_mul(100)
        .checked_div(i128::from(limit))
        .unwrap_or(0)
        .clamp(0, 100);
    let remaining_percentage = u8::try_from(percentage).unwrap_or(100);
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-quota-view:p1:inst-view-percentage
    Some(QuotaPeriodView {
        period,
        limit_credits_micro: limit,
        used_credits_micro: used,
        remaining_credits_micro: remaining,
        remaining_percentage,
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-quota-view:p1:inst-view-reset
        next_reset: keys.next_reset(period),
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-quota-view:p1:inst-view-reset
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-quota-view:p1:inst-view-flags
        warning: remaining_percentage <= 100_u8.saturating_sub(warning_threshold_pct),
        exhausted: remaining_percentage == 0,
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-quota-view:p1:inst-view-flags
    })
}

/// Warnings mode of the quota view: one entry per tier and period with a
/// positive limit, `next_reset` only when `warning` or `exhausted` is true.
#[must_use]
pub fn build_warnings(
    limits: &UserLimits,
    rows: &[BucketRow],
    warning_threshold_pct: u8,
    keys: PeriodKeys,
) -> Vec<QuotaWarningView> {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-quota-view:p1:inst-return-warnings
    build_status_view(limits, rows, warning_threshold_pct, keys)
        .into_iter()
        .flat_map(|tier| {
            tier.periods
                .into_iter()
                .map(move |period| QuotaWarningView {
                    tier: tier.tier,
                    period: period.period,
                    remaining_percentage: period.remaining_percentage,
                    warning: period.warning,
                    exhausted: period.exhausted,
                    next_reset: (period.warning || period.exhausted).then_some(period.next_reset),
                })
        })
        .collect()
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-quota-view:p1:inst-return-warnings
}

#[cfg(test)]
#[path = "status_tests.rs"]
mod tests;
