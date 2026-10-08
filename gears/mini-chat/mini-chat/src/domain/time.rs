//! Timestamp normalization for database writes.
//!
//! On `SQLite`, `time::OffsetDateTime` is stored as RFC 3339 text and sqlx trims
//! trailing zero sub-second digits, so values with different fraction lengths do
//! not sort lexicographically in time order. Every timestamp the gear writes
//! goes through [`db_ts`] / [`db_now`], which forces a UTC offset and a
//! nanosecond component of `micros * 1000 + 1`. The fraction then always has
//! nine digits (so it is never trimmed and sorts correctly as text), and the
//! value keeps its microsecond precision on Postgres `TIMESTAMPTZ`; only the
//! trailing 1 ns marker is lost there.

use time::{OffsetDateTime, UtcOffset};

/// Normalize `t` for storage: UTC, nanosecond = `(ns / 1000) * 1000 + 1`.
#[must_use]
pub fn db_ts(t: OffsetDateTime) -> OffsetDateTime {
    let t = t.to_offset(UtcOffset::UTC);
    let nanos = t.nanosecond() - t.nanosecond() % 1000 + 1;
    // `nanos` is at most 999_999_001, so this never fails; keep `t` otherwise.
    t.replace_nanosecond(nanos).unwrap_or(t)
}

/// Current time, normalized with [`db_ts`].
#[must_use]
pub fn db_now() -> OffsetDateTime {
    db_ts(OffsetDateTime::now_utc())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use time::{Duration, OffsetDateTime, UtcOffset, macros::datetime};

    use super::{db_now, db_ts};

    #[test]
    fn db_ts_forces_micros_plus_one_nanosecond() {
        let t = datetime!(2026-10-04 12:00:00.123_456_789 UTC);
        assert_eq!(db_ts(t).nanosecond(), 123_456_001);
        let whole = datetime!(2026-10-04 12:00:00 UTC);
        assert_eq!(db_ts(whole).nanosecond(), 1);
    }

    #[test]
    fn db_ts_converts_to_utc() {
        let t = datetime!(2026-10-04 14:00:00 +02:00);
        let out = db_ts(t);
        assert_eq!(out.offset(), UtcOffset::UTC);
        assert_eq!(out.hour(), 12);
    }

    #[test]
    fn db_ts_is_idempotent_and_monotonic() {
        let a = datetime!(2026-10-04 12:00:00.100_000_000 UTC);
        let b = a + Duration::nanoseconds(100);
        assert_eq!(db_ts(db_ts(a)), db_ts(a));
        assert!(db_ts(a) <= db_ts(b));
        assert!(db_ts(a) < db_ts(a + Duration::microseconds(1)));
    }

    #[test]
    fn db_now_is_normalized_utc() {
        let now = db_now();
        assert_eq!(now.offset(), UtcOffset::UTC);
        assert_eq!(now.nanosecond() % 1000, 1);
        assert!((OffsetDateTime::now_utc() - now).abs() < Duration::seconds(5));
    }
}
