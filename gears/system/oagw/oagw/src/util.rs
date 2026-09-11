//! Small shared helpers: RFC 3339 timestamps without a date-time dependency,
//! and the hop-by-hop / routing header vocabulary from `DESIGN.md`.

use std::time::{SystemTime, UNIX_EPOCH};

/// Headers OAGW consumes during routing and never forwards.
pub const ROUTING_HEADERS: &[&str] = &["x-oagw-target-host"];

/// Hop-by-hop headers, stripped in both directions (RFC 9110 §7.6.1 plus the
/// list tabulated under "Headers Transformation").
pub const HOP_BY_HOP_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Response header carrying the error attribution (ADR-0007).
pub const ERROR_SOURCE_HEADER: &str = "x-oagw-error-source";
/// Request header selecting a specific endpoint of a multi-endpoint upstream.
pub const TARGET_HOST_HEADER: &str = "x-oagw-target-host";
/// Correlation header propagated by the `request_id` transform plugin.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// Whether `name` (any case) must be stripped before forwarding.
#[must_use]
pub fn is_hop_by_hop(name: &str) -> bool {
    HOP_BY_HOP_HEADERS
        .iter()
        .any(|h| name.eq_ignore_ascii_case(h))
}

/// Whether `name` (any case) is consumed by OAGW's own routing.
#[must_use]
pub fn is_routing_header(name: &str) -> bool {
    ROUTING_HEADERS.iter().any(|h| name.eq_ignore_ascii_case(h))
}

/// Current time as an RFC 3339 UTC timestamp with second precision.
#[must_use]
pub fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    format_rfc3339(secs)
}

/// Seconds since the Unix epoch.
#[must_use]
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Render `epoch_secs` as `YYYY-MM-DDTHH:MM:SSZ`.
///
/// Uses the civil-from-days algorithm (Howard Hinnant, `chrono`-free) so the
/// gear does not need a date-time dependency for two audit columns.
#[must_use]
pub fn format_rfc3339(epoch_secs: u64) -> String {
    let days = i64::try_from(epoch_secs / 86_400).unwrap_or(0);
    let rem = epoch_secs % 86_400;
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Days since 1970-01-01 → `(year, month, day)`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    (
        year,
        u32::try_from(m).unwrap_or(1),
        u32::try_from(d).unwrap_or(1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_renders_correctly() {
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_rfc3339(1_000_000_000), "2001-09-09T01:46:40Z");
        // 2024-02-29 (leap day) 12:34:56 UTC
        assert_eq!(format_rfc3339(1_709_210_096), "2024-02-29T12:34:56Z");
    }

    #[test]
    fn now_is_well_formed_and_recent() {
        let now = now_rfc3339();
        assert_eq!(now.len(), 20, "{now}");
        assert!(now.ends_with('Z'));
        assert!(now.starts_with("20"), "{now}");
    }

    #[test]
    fn hop_by_hop_matching_is_case_insensitive() {
        assert!(is_hop_by_hop("Transfer-Encoding"));
        assert!(is_hop_by_hop("UPGRADE"));
        assert!(!is_hop_by_hop("content-type"));
        assert!(is_routing_header("X-OAGW-Target-Host"));
        assert!(!is_routing_header("x-request-id"));
    }
}
