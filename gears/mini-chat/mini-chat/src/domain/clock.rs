//! Application clock.
//!
//! SQLite stores timestamps as RFC 3339 text and compares them as text. The
//! encoders drop trailing zeros of the fractional second, which would make
//! text order differ from time order. Every timestamp written by the gear
//! therefore has microsecond precision plus one nanosecond, so the fraction is
//! always nine digits wide and text order equals time order.

use chrono::{DateTime, Datelike, NaiveDate, Timelike, Utc};

/// Current UTC time with a fixed-width fraction (see module docs).
#[must_use]
pub fn now() -> DateTime<Utc> {
    normalize(Utc::now())
}

/// Normalizes a timestamp to microsecond precision plus one nanosecond.
#[must_use]
#[allow(clippy::integer_division)] // floor to whole microseconds is intended
pub fn normalize(ts: DateTime<Utc>) -> DateTime<Utc> {
    let nanos = ts.nanosecond() % 1_000_000_000;
    let fixed = nanos / 1000 * 1000 + 1;
    ts.with_nanosecond(fixed).unwrap_or(ts)
}

/// Start of the UTC day of `ts`.
#[must_use]
pub fn day_start(ts: DateTime<Utc>) -> NaiveDate {
    ts.date_naive()
}

/// First day of the UTC month of `ts`.
#[must_use]
pub fn month_start(ts: DateTime<Utc>) -> NaiveDate {
    let d = ts.date_naive();
    NaiveDate::from_ymd_opt(d.year(), d.month(), 1).unwrap_or(d)
}

/// Midnight UTC after the day of `date`.
#[must_use]
pub fn next_daily_reset(date: NaiveDate) -> DateTime<Utc> {
    date.succ_opt()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map_or_else(Utc::now, |n| n.and_utc())
}

/// Midnight UTC on the first day of the month after `month_start`.
#[must_use]
pub fn next_monthly_reset(month_start: NaiveDate) -> DateTime<Utc> {
    let (y, m) = if month_start.month() == 12 {
        (month_start.year() + 1, 1)
    } else {
        (month_start.year(), month_start.month() + 1)
    };
    NaiveDate::from_ymd_opt(y, m, 1)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map_or_else(Utc::now, |n| n.and_utc())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_timestamps_have_nine_fraction_digits_and_sort_textually() {
        let a = normalize(DateTime::from_timestamp(1_700_000_000, 0).expect("ts"));
        let b = normalize(DateTime::from_timestamp(1_700_000_000, 500_000_000).expect("ts"));
        let c = normalize(DateTime::from_timestamp(1_700_000_000, 510_000_000).expect("ts"));
        let fa = a.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, false);
        let fb = b.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, false);
        let fc = c.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, false);
        assert_eq!(fa.len(), fb.len());
        assert!(fa < fb && fb < fc);
    }

    #[test]
    fn period_boundaries() {
        let ts = DateTime::parse_from_rfc3339("2026-02-28T23:59:59Z")
            .expect("parse")
            .with_timezone(&Utc);
        assert_eq!(day_start(ts).to_string(), "2026-02-28");
        assert_eq!(month_start(ts).to_string(), "2026-02-01");
        assert_eq!(
            next_daily_reset(day_start(ts)).to_rfc3339(),
            "2026-03-01T00:00:00+00:00"
        );
        let dec = NaiveDate::from_ymd_opt(2026, 12, 1).expect("date");
        assert_eq!(next_monthly_reset(dec).to_rfc3339(), "2027-01-01T00:00:00+00:00");
    }
}
