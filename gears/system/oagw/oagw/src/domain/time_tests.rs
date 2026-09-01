//! Tests for the RFC 3339 helpers (ordering property used by `$orderby`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::{civil_from_days, days_from_civil, days_in_month, format_epoch_millis, parse_rfc3339};

#[test]
fn formats_known_instants() {
    assert_eq!(format_epoch_millis(0), "1970-01-01T00:00:00.000Z");
    // 2026-08-29T00:00:00Z
    let millis = parse_rfc3339("2026-08-29T12:34:56.789Z").unwrap();
    assert_eq!(format_epoch_millis(millis), "2026-08-29T12:34:56.789Z");
}

#[test]
fn rendering_is_chronologically_ordered() {
    let mut stamps = Vec::new();
    for (year, month, day) in [(2025, 12, 31), (2026, 1, 1), (2026, 8, 29), (1970, 1, 1)] {
        for (hour, minute) in [(0, 0), (9, 30), (23, 59)] {
            let text = format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:00.000Z");
            let millis = parse_rfc3339(&text).unwrap();
            stamps.push((millis, format_epoch_millis(millis)));
        }
    }
    stamps.sort_by_key(|(millis, _)| *millis);
    let rendered: Vec<String> = stamps.into_iter().map(|(_, text)| text).collect();
    let mut sorted = rendered.clone();
    sorted.sort();
    assert_eq!(rendered, sorted);
}

#[test]
fn rejects_malformed_timestamps() {
    for text in [
        "",
        "2026-08-29",
        "2026-08-29T12:34:56Z",
        "2026-13-01T00:00:00.000Z",
        "2026-02-30T00:00:00.000Z",
        "2026-08-29T25:00:00.000Z",
        "2026-08-29T12:60:00.000Z",
        "2026-08-29T12:00:60.000Z",
        "2026-08-29T12:00:00.00Z",
        "XXXX-08-29T12:00:00.000Z",
    ] {
        assert!(
            parse_rfc3339(text).is_err(),
            "expected '{text}' to be rejected"
        );
    }
}

#[test]
fn accepts_leap_day() {
    let millis = parse_rfc3339("2024-02-29T00:00:00.000Z").unwrap();
    assert_eq!(format_epoch_millis(millis), "2024-02-29T00:00:00.000Z");
    assert!(parse_rfc3339("2100-02-29T00:00:00.000Z").is_err());
}

#[test]
fn civil_round_trip() {
    for (year, month, day) in [(1970, 1, 1), (2000, 2, 29), (2026, 8, 29), (9999, 12, 31)] {
        let days = days_from_civil(year, month, day);
        assert_eq!(civil_from_days(days), (year, month, day));
    }
}

#[test]
fn month_lengths() {
    assert_eq!(days_in_month(2026, 1), 31);
    assert_eq!(days_in_month(2026, 2), 28);
    assert_eq!(days_in_month(2024, 2), 29);
    assert_eq!(days_in_month(2026, 4), 30);
}
