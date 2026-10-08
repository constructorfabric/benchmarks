//! Clock helper.
//!
//! Timestamps are stored as `chrono::DateTime<Utc>`. On SQLite they are RFC
//! 3339 TEXT (`...+00:00`), the same encoding the `OData` filter layer binds,
//! so comparisons against client-supplied values are exact. The text has a
//! variable number of fractional digits, so lexical order could disagree with
//! time order (e.g. `...00.12+00:00` after `...00.123+00:00`); every
//! timestamp the gear writes has a non-zero last nanosecond digit, which keeps
//! the encoding at 9 fractional digits and the lexical order chronological.

use chrono::{DateTime, Utc};
use time::OffsetDateTime;

/// Persisted timestamp type.
pub type Timestamp = DateTime<Utc>;

/// Current UTC time normalized to a fixed 9-digit fraction.
#[must_use]
pub fn now() -> Timestamp {
    normalize(from_time(OffsetDateTime::now_utc()))
}

/// Normalize a timestamp (last nanosecond digit set to 1).
#[must_use]
pub fn normalize(ts: Timestamp) -> Timestamp {
    let nanos = ts.timestamp_nanos_opt().unwrap_or(0);
    let fixed = nanos - nanos.rem_euclid(10) + 1;
    DateTime::from_timestamp_nanos(fixed)
}

/// `time` -> `chrono`.
#[must_use]
pub fn from_time(ts: OffsetDateTime) -> Timestamp {
    let n = i64::try_from(ts.unix_timestamp_nanos()).unwrap_or(i64::MAX);
    DateTime::from_timestamp_nanos(n)
}

/// `chrono` -> `time` (SDK event timestamps, period math).
#[must_use]
pub fn to_time(ts: Timestamp) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ts.timestamp_nanos_opt().unwrap_or(0)))
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

/// RFC 3339 rendering of a `time` value (UTC).
#[must_use]
pub fn format_rfc3339(ts: OffsetDateTime) -> String {
    ts.to_offset(time::UtcOffset::UTC)
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::SecondsFormat;

    #[test]
    fn always_nine_fraction_digits() {
        for ns in [0_i64, 100_000_000, 120_000_000, 999_999_999] {
            let ts = DateTime::from_timestamp_nanos(1_700_000_000_000_000_000 + ns);
            let s = normalize(ts).to_rfc3339_opts(SecondsFormat::AutoSi, false);
            let frac = s.split('.').nth(1).unwrap();
            assert_eq!(frac.len(), 9 + 6, "{s}"); // 9 digits + "+00:00"
        }
    }

    #[test]
    fn lexical_order_matches_time_order() {
        let a = normalize(DateTime::from_timestamp_nanos(120_000_000));
        let b = normalize(DateTime::from_timestamp_nanos(123_000_000));
        assert!(a < b);
        assert!(a.to_rfc3339_opts(SecondsFormat::AutoSi, false) < b.to_rfc3339_opts(SecondsFormat::AutoSi, false));
    }

    #[test]
    fn time_roundtrip() {
        let n = now();
        assert_eq!(from_time(to_time(n)), n);
    }
}
