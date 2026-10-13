//! Timestamp normalisation for `SQLite` text columns.
//!
//! sqlx writes `time::OffsetDateTime` to `SQLite` as RFC 3339 text, and the `time` encoder trims
//! trailing zeros of the fraction (`.5Z`, `.55Z`). Text order is then not time order within one
//! second (`.5Z` sorts after `.55Z`), which breaks `ORDER BY`, cursors and cutoff comparisons.
//! Every timestamp the gear writes or binds goes through [`normalize`]: UTC, microsecond
//! precision (the precision of `PostgreSQL`) plus one nanosecond, so the encoder always prints
//! exactly nine fractional digits and text order equals time order. The `SQLite` column
//! defaults of the initial migration have the same shape.

use time::{OffsetDateTime, UtcOffset};

/// UTC, truncated to microseconds, plus 1 ns (see the module docs).
#[must_use]
pub fn normalize(t: OffsetDateTime) -> OffsetDateTime {
    let t = t.to_offset(UtcOffset::UTC);
    let micros_in_nanos = t.nanosecond() - t.nanosecond() % 1_000;
    // `micros_in_nanos + 1` is below 1_000_000_000: the largest microsecond value is 999_999_000.
    t.replace_nanosecond(micros_in_nanos + 1).unwrap_or(t)
}

/// The current time, [`normalize`]d.
#[must_use]
pub fn db_now() -> OffsetDateTime {
    normalize(OffsetDateTime::now_utc())
}

#[cfg(test)]
mod tests {
    use time::format_description::well_known::Rfc3339;
    use time::macros::datetime;

    use super::*;

    #[test]
    fn normalized_values_always_print_nine_fraction_digits() {
        for nanos in [0, 1, 500_000_000, 550_000_000, 123_456_789, 999_999_999] {
            let t = normalize(
                datetime!(2026-10-04 12:00:00 UTC)
                    .replace_nanosecond(nanos)
                    .unwrap(),
            );
            let text = t.format(&Rfc3339).unwrap();
            let fraction = text.split('.').nth(1).unwrap().trim_end_matches('Z');
            assert_eq!(fraction.len(), 9, "{text}");
        }
    }

    #[test]
    fn normalize_truncates_to_micros_converts_to_utc_and_is_idempotent() {
        let t = datetime!(2026-10-04 14:00:00.123_456_789 +02:00);
        let n = normalize(t);
        assert_eq!(n, datetime!(2026-10-04 12:00:00.123_456_001 UTC));
        assert_eq!(n.offset(), UtcOffset::UTC);
        assert_eq!(normalize(n), n);
    }

    #[test]
    fn text_order_is_time_order_within_one_second() {
        let at = |nanos| {
            normalize(
                datetime!(2026-10-04 12:00:00 UTC)
                    .replace_nanosecond(nanos)
                    .unwrap(),
            )
        };
        let mut times = [
            at(550_000_000),
            at(500_000_000),
            at(0),
            at(999_999_000),
            at(50_000_000),
        ];
        let mut by_text = times.map(|t| (t.format(&Rfc3339).unwrap(), t));
        times.sort();
        by_text.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(times, by_text.map(|(_, t)| t));
    }
}
