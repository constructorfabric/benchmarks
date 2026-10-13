//! Quota warning / status math (DESIGN §3.2 "Quota Warning Thresholds", "Quota Status Endpoint").

use mini_chat_sdk::UserLimits;
use time::{Date, Month, OffsetDateTime, Time};

use super::QuotaPeriods;
use super::store::{Bucket, PeriodKind, UsageRows};
use crate::api::rest::dto::{QuotaPeriod, QuotaPeriodStatus, QuotaTier, QuotaTierStatus, QuotaWarning};

/// Next reset instant of a period started at `periods` (midnight UTC of the next day / of the
/// 1st of the next month).
#[must_use]
pub fn next_reset(period: PeriodKind, periods: &QuotaPeriods) -> OffsetDateTime {
    let date = match period {
        PeriodKind::Daily => periods
            .daily_start
            .next_day()
            .unwrap_or(periods.daily_start),
        PeriodKind::Monthly => {
            let m = periods.monthly_start;
            let (year, month) = if m.month() == Month::December {
                (m.year() + 1, Month::January)
            } else {
                (m.year(), m.month().next())
            };
            Date::from_calendar_date(year, month, 1).unwrap_or(m)
        }
    };
    date.with_time(Time::MIDNIGHT).assume_utc()
}

/// Computed status of one (bucket, period).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeriodFigures {
    pub limit: i64,
    pub used: i64,
    pub remaining: i64,
    pub remaining_percentage: u32,
    pub warning: bool,
    pub exhausted: bool,
}

/// `used = spent + reserved`, `remaining = max(limit - used, 0)`,
/// `pct = floor(remaining * 100 / limit)`; `None` when `limit <= 0` (period skipped).
#[must_use]
pub fn figures(limit: i64, used: i64, warning_threshold_pct: u8) -> Option<PeriodFigures> {
    if limit <= 0 {
        return None;
    }
    let remaining = limit.saturating_sub(used).max(0);
    let pct = i128::from(remaining) * 100 / i128::from(limit);
    let remaining_percentage = u32::try_from(pct.clamp(0, 100)).unwrap_or(0);
    let warning = remaining_percentage <= 100 - u32::from(warning_threshold_pct.min(100));
    Some(PeriodFigures {
        limit,
        used,
        remaining,
        remaining_percentage,
        warning,
        exhausted: remaining_percentage == 0,
    })
}

/// Limit of a bucket for a period (`total` → standard limits, `tier:premium` → premium limits).
#[must_use]
pub const fn limit_of(limits: &UserLimits, bucket: Bucket, period: PeriodKind) -> i64 {
    let tier = match bucket {
        Bucket::Total => &limits.standard,
        Bucket::Premium => &limits.premium,
    };
    match period {
        PeriodKind::Daily => tier.limit_daily_credits_micro,
        PeriodKind::Monthly => tier.limit_monthly_credits_micro,
    }
}

const fn dto_period(p: PeriodKind) -> QuotaPeriod {
    match p {
        PeriodKind::Daily => QuotaPeriod::Daily,
        PeriodKind::Monthly => QuotaPeriod::Monthly,
    }
}

const fn dto_tier(b: Bucket) -> QuotaTier {
    match b {
        Bucket::Total => QuotaTier::Total,
        Bucket::Premium => QuotaTier::Premium,
    }
}

/// Tier order of the API: premium, then total.
const TIERS: [Bucket; 2] = [Bucket::Premium, Bucket::Total];

/// Per-tier status (periods with `limit <= 0` skipped; a tier without periods is omitted).
#[must_use]
pub fn tier_statuses(
    limits: &UserLimits,
    rows: &UsageRows,
    periods: &QuotaPeriods,
    warning_threshold_pct: u8,
) -> Vec<QuotaTierStatus> {
    TIERS
        .iter()
        .filter_map(|&bucket| {
            let list: Vec<QuotaPeriodStatus> = PeriodKind::ALL
                .iter()
                .filter_map(|&p| {
                    let f = figures(
                        limit_of(limits, bucket, p),
                        rows.used(periods, p, bucket),
                        warning_threshold_pct,
                    )?;
                    Some(QuotaPeriodStatus {
                        period: dto_period(p),
                        limit_credits_micro: f.limit,
                        used_credits_micro: f.used,
                        remaining_credits_micro: f.remaining,
                        remaining_percentage: f.remaining_percentage,
                        next_reset: next_reset(p, periods),
                        warning: f.warning,
                        exhausted: f.exhausted,
                    })
                })
                .collect();
            (!list.is_empty()).then_some(QuotaTierStatus {
                tier: dto_tier(bucket),
                periods: list,
            })
        })
        .collect()
}

/// `done.quota_warnings` entries (`next_reset` only when `warning` or `exhausted`).
#[must_use]
pub fn warnings(
    limits: &UserLimits,
    rows: &UsageRows,
    periods: &QuotaPeriods,
    warning_threshold_pct: u8,
) -> Vec<QuotaWarning> {
    let mut out = Vec::new();
    for bucket in TIERS {
        for p in PeriodKind::ALL {
            let Some(f) = figures(
                limit_of(limits, bucket, p),
                rows.used(periods, p, bucket),
                warning_threshold_pct,
            ) else {
                continue;
            };
            out.push(QuotaWarning {
                tier: dto_tier(bucket),
                period: dto_period(p),
                remaining_percentage: f.remaining_percentage,
                warning: f.warning,
                exhausted: f.exhausted,
                next_reset: (f.warning || f.exhausted).then(|| next_reset(p, periods)),
            });
        }
    }
    out
}
