//! UTC calendar quota periods (DESIGN §3.2 "Quota Period Reset Semantics").

use time::{Date, Duration, Month, OffsetDateTime, Time, UtcOffset};

/// Quota period type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PeriodType {
    Daily,
    Monthly,
}

impl PeriodType {
    pub const ALL: [Self; 2] = [Self::Daily, Self::Monthly];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Monthly => "monthly",
        }
    }
}

/// Period start of `ts` (UTC date for daily, 1st of the month for monthly).
#[must_use]
pub fn period_start(period: PeriodType, ts: OffsetDateTime) -> Date {
    let d = ts.to_offset(UtcOffset::UTC).date();
    match period {
        PeriodType::Daily => d,
        PeriodType::Monthly => d.replace_day(1).unwrap_or(d),
    }
}

/// The next reset instant after `ts` (midnight UTC tomorrow / 1st of next month).
#[must_use]
pub fn next_reset(period: PeriodType, ts: OffsetDateTime) -> OffsetDateTime {
    let d = ts.to_offset(UtcOffset::UTC).date();
    let next = match period {
        PeriodType::Daily => d.next_day().unwrap_or(d),
        PeriodType::Monthly => {
            let (y, m) = if d.month() == Month::December {
                (d.year() + 1, Month::January)
            } else {
                (d.year(), d.month().next())
            };
            Date::from_calendar_date(y, m, 1).unwrap_or(d)
        }
    };
    next.with_time(Time::MIDNIGHT).assume_utc()
}

/// Period starts derived from a turn's `started_at` (orphan watchdog path).
#[must_use]
pub fn starts_for(ts: OffsetDateTime) -> (Date, Date) {
    (period_start(PeriodType::Daily, ts), period_start(PeriodType::Monthly, ts))
}

/// Seconds helper used by workers.
#[must_use]
pub fn secs(n: u64) -> Duration {
    Duration::seconds(i64::try_from(n).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(s: &str) -> OffsetDateTime {
        OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).unwrap()
    }

    #[test]
    fn period_starts() {
        let ts = dt("2026-02-28T15:30:00Z");
        assert_eq!(period_start(PeriodType::Daily, ts).to_string(), "2026-02-28");
        assert_eq!(period_start(PeriodType::Monthly, ts).to_string(), "2026-02-01");
        let late = dt("2026-02-28T23:59:59Z");
        assert_eq!(period_start(PeriodType::Daily, late).to_string(), "2026-02-28");
    }

    #[test]
    fn next_resets() {
        let ts = dt("2026-12-31T10:00:00Z");
        assert_eq!(next_reset(PeriodType::Daily, ts), dt("2027-01-01T00:00:00Z"));
        assert_eq!(next_reset(PeriodType::Monthly, ts), dt("2027-01-01T00:00:00Z"));
        let mid = dt("2026-02-10T10:00:00Z");
        assert_eq!(next_reset(PeriodType::Monthly, mid), dt("2026-03-01T00:00:00Z"));
    }
}
