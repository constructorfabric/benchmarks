//! Minimal RFC 3339 formatting over [`SystemTime`].
//!
//! OAGW carries no date/time crate; audit timestamps and `gc_eligible_at`
//! only need a stable, sortable UTC rendering, which is a dozen lines of
//! civil-calendar arithmetic.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Seconds in a day.
const DAY: i64 = 86_400;

/// Current instant as an RFC 3339 UTC timestamp (second precision).
#[must_use]
pub fn now_rfc3339() -> String {
    to_rfc3339(SystemTime::now())
}

/// `SystemTime` as an RFC 3339 UTC timestamp (second precision).
#[must_use]
pub fn to_rfc3339(at: SystemTime) -> String {
    let secs = match at.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_secs()).unwrap_or(i64::MAX),
    };
    let days = secs.div_euclid(DAY);
    let rem = secs.rem_euclid(DAY);
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// The instant `after` from now, as an RFC 3339 UTC timestamp.
#[must_use]
pub fn rfc3339_in(after: Duration) -> String {
    to_rfc3339(SystemTime::now() + after)
}

/// Howard Hinnant's `civil_from_days`: days since the Unix epoch to
/// (year, month, day) in the proleptic Gregorian calendar.
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
    let year = if m <= 2 { y + 1 } else { y };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    (year, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::{now_rfc3339, to_rfc3339};
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn epoch_formats() {
        assert_eq!(to_rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn known_instants_format() {
        // 2001-09-09T01:46:40Z — the 1e9 second mark.
        assert_eq!(
            to_rfc3339(UNIX_EPOCH + Duration::from_secs(1_000_000_000)),
            "2001-09-09T01:46:40Z"
        );
        // A leap day.
        assert_eq!(
            to_rfc3339(UNIX_EPOCH + Duration::from_secs(1_583_020_800)),
            "2020-03-01T00:00:00Z"
        );
    }

    #[test]
    fn now_is_sortable_and_well_formed() {
        let a = now_rfc3339();
        assert_eq!(a.len(), 20, "fixed-width rendering keeps string sort valid");
        assert!(a.ends_with('Z'));
        assert!(a.as_str() > "2020-01-01T00:00:00Z");
    }
}
