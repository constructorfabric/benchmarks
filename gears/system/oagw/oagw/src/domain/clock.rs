//! Timestamps.
//!
//! The domain model carries RFC 3339 timestamps in UTC. Formatting them by
//! hand keeps the gear free of a wall-clock dependency: the conversion from a
//! Unix day number to a civil date is the standard Howard Hinnant algorithm,
//! and it is unit-tested against a set of known instants.

use std::time::{SystemTime, UNIX_EPOCH};

/// The current UTC time, formatted as RFC 3339 with millisecond precision.
#[must_use]
pub fn now_rfc3339() -> String {
    format_rfc3339(SystemTime::now())
}

/// Format a [`SystemTime`] as an RFC 3339 UTC timestamp.
#[must_use]
pub fn format_rfc3339(time: SystemTime) -> String {
    let millis = time
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis() as i64)
        .unwrap_or_default();
    format_millis(millis)
}

/// Format a Unix epoch in milliseconds as RFC 3339 UTC.
#[must_use]
pub fn format_millis(millis: i64) -> String {
    let seconds = millis.div_euclid(1000);
    let millisecond = millis.rem_euclid(1000);
    let days = seconds.div_euclid(86_400);
    let secs_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millisecond:03}Z")
}

/// Convert a Unix day number into a `(year, month, day)` triple.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_the_epoch() {
        assert_eq!(format_millis(0), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn formats_a_known_instant() {
        // 2026-02-03T11:09:37.431Z, the instant used in ADR 0001's audit log.
        assert_eq!(format_millis(1_770_116_977_431), "2026-02-03T11:09:37.431Z");
    }

    #[test]
    fn formats_a_leap_day() {
        assert_eq!(format_millis(951_782_400_000), "2000-02-29T00:00:00.000Z");
    }

    #[test]
    fn handles_pre_epoch_values() {
        assert_eq!(format_millis(-1), "1969-12-31T23:59:59.999Z");
    }

    #[test]
    fn formats_day_boundaries() {
        assert_eq!(format_millis(86_399_999), "1970-01-01T23:59:59.999Z");
        assert_eq!(format_millis(86_400_000), "1970-01-02T00:00:00.000Z");
    }
}
