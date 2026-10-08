//! Timestamp helper.
//!
//! Every timestamp the gear writes comes from [`now`]: the UTC wall clock
//! truncated to microseconds, made strictly increasing within the process,
//! plus one nanosecond. The trailing nanosecond makes every value carry nine
//! fractional digits when rendered as RFC 3339, so the textual form stored by
//! SQLite sorts exactly like the instant (no trimmed trailing zeros), and
//! cursor values round-trip byte-for-byte.

use std::sync::atomic::{AtomicI64, Ordering};

use time::{Date, Duration, OffsetDateTime, UtcOffset};

static LAST_MICROS: AtomicI64 = AtomicI64::new(0);

/// Current UTC time (microsecond precision, strictly increasing, +1 ns).
#[must_use]
pub fn now() -> OffsetDateTime {
    let wall = OffsetDateTime::now_utc();
    let micros = i64::try_from(wall.unix_timestamp_nanos().div_euclid(1_000)).unwrap_or(i64::MAX);
    let mut prev = LAST_MICROS.load(Ordering::Relaxed);
    let chosen = loop {
        let candidate = if micros > prev { micros } else { prev + 1 };
        match LAST_MICROS.compare_exchange_weak(
            prev,
            candidate,
            Ordering::SeqCst,
            Ordering::Relaxed,
        ) {
            Ok(_) => break candidate,
            Err(actual) => prev = actual,
        }
    };
    from_micros(chosen)
}

/// Normalize an arbitrary instant to the stored representation (UTC,
/// microsecond precision, +1 ns).
#[must_use]
pub fn normalize(ts: OffsetDateTime) -> OffsetDateTime {
    let micros = i64::try_from(
        ts.to_offset(UtcOffset::UTC)
            .unix_timestamp_nanos()
            .div_euclid(1_000),
    )
    .unwrap_or(i64::MAX);
    from_micros(micros)
}

fn from_micros(micros: i64) -> OffsetDateTime {
    let base = OffsetDateTime::from_unix_timestamp_nanos(i128::from(micros) * 1_000)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH);
    base + Duration::nanoseconds(1)
}

/// Stored textual form: RFC 3339 UTC with exactly nine fractional digits
/// (the form SQLite holds for values produced by [`now`]).
#[must_use]
pub fn format_stored(ts: OffsetDateTime) -> String {
    let ts = ts.to_offset(UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:09}Z",
        ts.year(),
        u8::from(ts.month()),
        ts.day(),
        ts.hour(),
        ts.minute(),
        ts.second(),
        ts.nanosecond()
    )
}

/// API form: RFC 3339 UTC with exactly six fractional digits.
#[must_use]
pub fn format_api(ts: OffsetDateTime) -> String {
    let ts = ts.to_offset(UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}Z",
        ts.year(),
        u8::from(ts.month()),
        ts.day(),
        ts.hour(),
        ts.minute(),
        ts.second(),
        ts.microsecond()
    )
}

/// UTC calendar day of an instant.
#[must_use]
pub fn utc_date(ts: OffsetDateTime) -> Date {
    ts.to_offset(UtcOffset::UTC).date()
}

/// First day of the UTC month of an instant.
#[must_use]
pub fn utc_month_start(ts: OffsetDateTime) -> Date {
    let d = utc_date(ts);
    Date::from_calendar_date(d.year(), d.month(), 1).unwrap_or(d)
}

/// Midnight UTC of the next day.
#[must_use]
pub fn next_daily_reset(ts: OffsetDateTime) -> OffsetDateTime {
    let d = utc_date(ts);
    let next = d.next_day().unwrap_or(d);
    next.midnight().assume_utc()
}

/// Midnight UTC of the first day of the next month.
#[must_use]
pub fn next_monthly_reset(ts: OffsetDateTime) -> OffsetDateTime {
    let d = utc_date(ts);
    let (year, month) = if d.month() == time::Month::December {
        (d.year() + 1, time::Month::January)
    } else {
        (d.year(), d.month().next())
    };
    Date::from_calendar_date(year, month, 1)
        .unwrap_or(d)
        .midnight()
        .assume_utc()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_is_strictly_increasing_and_has_nine_digits() {
        let a = now();
        let b = now();
        assert!(b > a);
        let s = a
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();
        let frac = s.split('.').nth(1).unwrap_or_default();
        assert_eq!(frac.len(), 10, "nine digits plus 'Z': {s}");
    }

    #[test]
    fn resets_are_calendar_based_utc() {
        let ts = time::macros::datetime!(2026-02-28 15:30:00 UTC);
        assert_eq!(utc_date(ts), time::macros::date!(2026 - 02 - 28));
        assert_eq!(utc_month_start(ts), time::macros::date!(2026 - 02 - 01));
        assert_eq!(
            next_daily_reset(ts),
            time::macros::datetime!(2026-03-01 00:00:00 UTC)
        );
        assert_eq!(
            next_monthly_reset(ts),
            time::macros::datetime!(2026-03-01 00:00:00 UTC)
        );
        let dec = time::macros::datetime!(2026-12-31 23:59:59 UTC);
        assert_eq!(
            next_monthly_reset(dec),
            time::macros::datetime!(2027-01-01 00:00:00 UTC)
        );
    }
}
