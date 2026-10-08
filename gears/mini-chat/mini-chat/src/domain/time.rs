//! Clock helpers.
//!
//! SQLite stores timestamps as RFC 3339 text, whose fractional part is trimmed
//! of trailing zeros. Variable-length fractions do not sort lexicographically
//! in time order, so every timestamp written by the gear has exactly nine
//! fractional digits: microsecond precision plus a non-zero nanosecond digit.

use time::{Duration, OffsetDateTime};

/// Current UTC time, normalized for storage (see module docs).
#[must_use]
pub fn now() -> OffsetDateTime {
    normalize(OffsetDateTime::now_utc())
}

/// Normalizes a timestamp to microsecond precision with a trailing `1` ns.
#[must_use]
#[allow(clippy::integer_division)] // truncation to whole microseconds is intended
pub fn normalize(t: OffsetDateTime) -> OffsetDateTime {
    let t = t.to_offset(time::UtcOffset::UTC);
    let micros = t.nanosecond() / 1_000;
    t.replace_nanosecond(micros * 1_000 + 1).unwrap_or(t)
}

/// `t - secs`.
#[must_use]
#[allow(clippy::integer_division)] // exact constant: overflow-safe saturation cap
pub fn minus_secs(t: OffsetDateTime, secs: u64) -> OffsetDateTime {
    t - Duration::seconds(i64::try_from(secs).unwrap_or(i64::MAX / 2))
}
