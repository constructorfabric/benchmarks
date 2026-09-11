//! Unit tests for identifier generation and timestamp formatting.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use std::time::Duration;

#[test]
fn generated_uuids_are_unique_and_well_formed() {
    let first = new_uuid();
    let second = new_uuid();
    assert_ne!(first, second);
    assert_eq!(first.to_string().len(), 36);
    assert_eq!(first.get_version_num(), 5);
}

#[test]
fn typed_ids_carry_the_prefix() {
    let id = typed_id("gts.cf.core.oagw.upstream.v1~");
    assert!(id.starts_with("gts.cf.core.oagw.upstream.v1~"));
    assert_eq!(id.split('~').count(), 2);
}

#[test]
fn epoch_formats_as_rfc3339() {
    assert_eq!(
        format_epoch(Duration::from_secs(0)),
        "1970-01-01T00:00:00.000Z"
    );
    assert_eq!(
        format_epoch(Duration::from_millis(1_700_000_000_123)),
        "2023-11-14T22:13:20.123Z"
    );
    assert_eq!(
        format_epoch(Duration::from_secs(1_760_000_000)),
        "2025-10-09T08:53:20.000Z"
    );
}

#[test]
fn leap_day_is_handled() {
    // 2024-02-29T00:00:00Z
    let secs = 1_709_164_800;
    assert_eq!(
        format_epoch(Duration::from_secs(secs)),
        "2024-02-29T00:00:00.000Z"
    );
}

#[test]
fn now_is_a_plausible_timestamp() {
    let now = now_rfc3339();
    assert_eq!(now.len(), 24);
    assert!(now.ends_with('Z'));
    assert_eq!(&now[4..5], "-");
    assert_eq!(&now[10..11], "T");
}
