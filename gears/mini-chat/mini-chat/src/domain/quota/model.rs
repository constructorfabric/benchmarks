//! Quota periods, buckets and usage snapshots (DESIGN section 3.7 `quota_usage`, 5.4.2).

use std::collections::HashMap;

use time::{Date, OffsetDateTime};
use toolkit_macros::domain_model;

/// A quota period type. P1 enables the daily and the monthly period.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PeriodType {
    /// UTC calendar day.
    Daily,
    /// UTC calendar month.
    Monthly,
}

impl PeriodType {
    /// Every enabled period type.
    pub const ALL: [Self; 2] = [Self::Daily, Self::Monthly];

    /// The stored `period_type` value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Monthly => "monthly",
        }
    }
}

/// A period instance: the type and its UTC start date.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct QuotaPeriod {
    /// Daily or monthly.
    pub kind: PeriodType,
    /// The first day of the period.
    pub start: Date,
}

impl QuotaPeriod {
    /// The periods containing `instant` (UTC).
    #[must_use]
    pub fn containing(instant: OffsetDateTime) -> [Self; 2] {
        let date = instant.to_offset(time::UtcOffset::UTC).date();
        [
            Self {
                kind: PeriodType::Daily,
                start: date,
            },
            Self {
                kind: PeriodType::Monthly,
                start: date.replace_day(1).unwrap_or(date),
            },
        ]
    }

    /// The first instant of the next period.
    #[must_use]
    pub fn next_reset(self) -> OffsetDateTime {
        let next = match self.kind {
            PeriodType::Daily => self.start.next_day(),
            PeriodType::Monthly => {
                let (year, month) = (self.start.year(), self.start.month());
                let next_month = month.next();
                let year = if next_month == time::Month::January {
                    year + 1
                } else {
                    year
                };
                Date::from_calendar_date(year, next_month, 1).ok()
            }
        };
        next.unwrap_or(self.start).midnight().assume_utc()
    }
}

/// A quota bucket: the overall cap or the premium sub-cap.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Bucket {
    /// The overall cap across all tiers (`user_limits.standard`).
    Total,
    /// The premium sub-cap (`user_limits.premium`).
    PremiumTier,
}

impl Bucket {
    /// The stored `bucket` value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Total => "total",
            Self::PremiumTier => "tier:premium",
        }
    }
}

/// The counters of one bucket row.
#[domain_model]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BucketUsage {
    /// Settled credits.
    pub spent_credits_micro: i64,
    /// In-flight reserves.
    pub reserved_credits_micro: i64,
    /// Web search calls (bucket `total` only).
    pub web_search_calls: i64,
    /// Code interpreter calls (bucket `total` only).
    pub code_interpreter_calls: i64,
}

/// The bucket rows of one user for the current periods; a missing row counts as zero.
#[domain_model]
#[derive(Debug, Clone, Default)]
pub struct QuotaSnapshot {
    rows: HashMap<(PeriodType, Bucket), BucketUsage>,
}

impl QuotaSnapshot {
    /// Builds a snapshot from rows.
    #[must_use]
    pub fn from_rows(rows: impl IntoIterator<Item = ((PeriodType, Bucket), BucketUsage)>) -> Self {
        Self {
            rows: rows.into_iter().collect(),
        }
    }

    /// The counters of a bucket row (zero when the row does not exist).
    #[must_use]
    pub fn usage(&self, period: PeriodType, bucket: Bucket) -> BucketUsage {
        self.rows
            .get(&(period, bucket))
            .copied()
            .unwrap_or_default()
    }
}

/// One limit to enforce when a reserve is written: bucket, period and the limit of the user.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BucketLimit {
    /// The bucket.
    pub bucket: Bucket,
    /// The period instance.
    pub period: QuotaPeriod,
    /// `limit_credits_micro` of the bucket in the period.
    pub limit_credits_micro: i64,
}

/// The buckets a reserve touches (the effective tier's) with their limits.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReservePlan {
    /// The reserve of the turn.
    pub reserved_credits_micro: i64,
    /// The periods containing the turn's start.
    pub periods: [QuotaPeriod; 2],
    /// Every bucket and period the reserve is booked in and re-checked against.
    pub limits: Vec<BucketLimit>,
}

/// One entry of `done.quota_warnings`.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaWarning {
    /// `premium` or `total`.
    pub tier: &'static str,
    /// `daily` or `monthly`.
    pub period: &'static str,
    /// Floored remaining percentage of the limit (0..=100).
    pub remaining_percentage: i64,
    /// `remaining_percentage <= 100 - warning_threshold_pct`.
    pub warning: bool,
    /// `remaining_percentage == 0`.
    pub exhausted: bool,
    /// The next reset, set only when `warning` or `exhausted`.
    pub next_reset: Option<OffsetDateTime>,
}

/// The remaining quota of one tier in one period.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeriodBreakdown {
    /// `premium` or `total`.
    pub tier: &'static str,
    /// `daily` or `monthly`.
    pub period: &'static str,
    /// The limit of the tier in the period (always positive).
    pub limit_credits_micro: i64,
    /// `spent + reserved`.
    pub used_credits_micro: i64,
    /// The limit minus the used credits, never negative.
    pub remaining_credits_micro: i64,
    /// Floored remaining percentage of the limit (0..=100).
    pub remaining_percentage: i64,
    /// `remaining_percentage <= 100 - warning_threshold_pct`.
    pub warning: bool,
    /// `remaining_percentage == 0`.
    pub exhausted: bool,
    /// The start of the next period.
    pub next_reset: OffsetDateTime,
}
