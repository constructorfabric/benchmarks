//! Minimal RFC 3339 timestamp formatting (no external date dependency).

/// Format a `SystemTime` offset from the epoch as RFC 3339 UTC with
/// millisecond precision.
#[must_use]
pub fn format_rfc3339(from_epoch: std::time::Duration) -> String {
    let secs = from_epoch.as_secs();
    let millis = from_epoch.subsec_millis();
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (y, mo, d) = civil_from_days(days as i64);
    format!(
        "{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{millis:03}Z",
        h = rem / 3600,
        mi = (rem % 3600) / 60,
        s = rem % 60
    )
}

/// Convert days since 1970-01-01 to a `(year, month, day)` triple.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
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
    use std::time::Duration;

    #[test]
    fn epoch_is_correct() {
        assert_eq!(format_rfc3339(Duration::from_secs(0)), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn known_timestamp() {
        // 2026-01-01T00:00:00Z == 1767225600
        assert_eq!(
            format_rfc3339(Duration::from_secs(1_767_225_600)),
            "2026-01-01T00:00:00.000Z"
        );
    }
}
