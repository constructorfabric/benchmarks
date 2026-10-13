//! Quota periods and buckets (DESIGN section 3.2 "Quota Period Reset Semantics", 5.4.2).
//!
//! All boundaries are UTC calendar boundaries: a day starts at 00:00 UTC, a month on the 1st at
//! 00:00 UTC. `quota_usage.period_start` stores the first day of the period as a DATE.

use time::{Date, OffsetDateTime, Time, UtcOffset};

/// Quota period (`quota_usage.period_type`); serializes as `daily` / `monthly`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Period {
    Daily,
    Monthly,
}

impl Period {
    /// Both enforced periods, in reporting order.
    pub const ALL: [Self; 2] = [Self::Daily, Self::Monthly];

    /// Value stored in `quota_usage.period_type`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Monthly => "monthly",
        }
    }
}

/// Quota bucket (`quota_usage.bucket`): `total` is the overall cap of all tiers (limits
/// `user_limits.standard`), `tier:premium` the premium subcap (limits `user_limits.premium`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Bucket {
    Total,
    Premium,
}

impl Bucket {
    /// Value stored in `quota_usage.bucket`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Total => "total",
            Self::Premium => "tier:premium",
        }
    }

    /// Tier name of the bucket in the quota status API and `quota_warnings`.
    #[must_use]
    pub const fn tier_name(self) -> &'static str {
        match self {
            Self::Total => "total",
            Self::Premium => "premium",
        }
    }
}

/// `period_start` of each period, computed once at preflight and reused at settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeriodStarts {
    pub daily: Date,
    pub monthly: Date,
}

impl PeriodStarts {
    /// The start date of `period`.
    #[must_use]
    pub const fn start(self, period: Period) -> Date {
        match period {
            Period::Daily => self.daily,
            Period::Monthly => self.monthly,
        }
    }
}

/// UTC day and month start of `now`.
#[must_use]
pub fn period_starts(now: OffsetDateTime) -> PeriodStarts {
    let today = now.to_offset(UtcOffset::UTC).date();
    PeriodStarts {
        daily: today,
        monthly: today.replace_day(1).unwrap_or(today),
    }
}

/// Start of the next period after `now`: 00:00 UTC tomorrow (daily) or 00:00 UTC on the 1st of
/// next month (monthly).
#[must_use]
pub fn next_reset(period: Period, now: OffsetDateTime) -> OffsetDateTime {
    let today = now.to_offset(UtcOffset::UTC).date();
    let next = match period {
        Period::Daily => today.next_day(),
        Period::Monthly => {
            let (year, month) = match today.month() {
                time::Month::December => (today.year() + 1, time::Month::January),
                m => (today.year(), m.next()),
            };
            Date::from_calendar_date(year, month, 1).ok()
        }
    }
    .unwrap_or(today);
    next.with_time(Time::MIDNIGHT).assume_utc()
}

#[cfg(test)]
mod tests {
    use time::macros::{date, datetime};

    use super::*;

    #[test]
    fn period_starts_are_utc_day_and_month() {
        let s = period_starts(datetime!(2026-02-28 15:30:00 UTC));
        assert_eq!(s.daily, date!(2026 - 02 - 28));
        assert_eq!(s.monthly, date!(2026 - 02 - 01));
        assert_eq!(
            period_starts(datetime!(2026-03-01 00:00:00 UTC)).daily,
            date!(2026 - 03 - 01)
        );
        // 2026-03-01 01:00 at +02:00 is still February 28 in UTC.
        let s = period_starts(datetime!(2026-03-01 01:00:00 +02:00));
        assert_eq!(
            (s.daily, s.monthly),
            (date!(2026 - 02 - 28), date!(2026 - 02 - 01))
        );
    }

    #[test]
    fn next_reset_is_next_midnight_and_next_first_of_month() {
        let now = datetime!(2026-10-04 15:30:00 UTC);
        assert_eq!(
            next_reset(Period::Daily, now),
            datetime!(2026-10-05 00:00:00 UTC)
        );
        assert_eq!(
            next_reset(Period::Monthly, now),
            datetime!(2026-11-01 00:00:00 UTC)
        );
        let dec = datetime!(2026-12-31 23:59:59 UTC);
        assert_eq!(
            next_reset(Period::Daily, dec),
            datetime!(2027-01-01 00:00:00 UTC)
        );
        assert_eq!(
            next_reset(Period::Monthly, dec),
            datetime!(2027-01-01 00:00:00 UTC)
        );
    }

    #[test]
    fn stored_names() {
        assert_eq!(
            (Period::Daily.as_str(), Period::Monthly.as_str()),
            ("daily", "monthly")
        );
        assert_eq!(
            (Bucket::Total.as_str(), Bucket::Premium.as_str()),
            ("total", "tier:premium")
        );
    }
}
