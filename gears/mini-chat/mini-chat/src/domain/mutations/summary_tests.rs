//! The summary frontier comparison of the summary invalidation: a frontier before, equal to and
//! after the target user message `(created_at, id)`, with the `id` tie-break and microsecond
//! precision.
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-turn-mutations-and-reactions`: algorithm
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-summary-invalidation` (`inst-si-covers`,
//! `inst-si-keep`, `inst-si-return-kept`); definition of done
//! `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-summary-invalidation`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use super::frontier_covers;

fn instant(micros: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap() + Duration::microseconds(micros)
}

fn id(value: u128) -> Uuid {
    Uuid::from_u128(value)
}

#[test]
fn a_frontier_before_the_target_does_not_cover_it() {
    // inst-si-keep: the summary stays
    assert!(!frontier_covers((instant(10), id(5)), (instant(20), id(5))));
}

#[test]
fn a_frontier_equal_to_the_target_covers_it() {
    // inst-si-covers: "at or after"
    assert!(frontier_covers((instant(20), id(5)), (instant(20), id(5))));
}

#[test]
fn a_frontier_after_the_target_covers_it() {
    // inst-si-covers: the summary of the deleted turn already covers the previous one
    assert!(frontier_covers((instant(30), id(1)), (instant(20), id(9))));
}

#[test]
fn at_the_same_created_at_the_message_id_breaks_the_tie() {
    // the order is `(created_at, id)`
    assert!(!frontier_covers((instant(20), id(4)), (instant(20), id(5))));
    assert!(frontier_covers((instant(20), id(6)), (instant(20), id(5))));
}

#[test]
fn a_microsecond_decides_the_comparison() {
    assert!(!frontier_covers(
        (instant(123_000), id(9)),
        (instant(123_001), id(1))
    ));
    assert!(frontier_covers(
        (instant(123_001), id(1)),
        (instant(123_000), id(9))
    ));
}

#[test]
fn the_comparison_is_chronological_across_offsets() {
    // the same instant expressed with another UTC offset is the same position
    let target = instant(500);
    let frontier = target.to_offset(time::UtcOffset::from_hms(5, 30, 0).unwrap());
    assert!(frontier_covers((frontier, id(7)), (target, id(7))));
}
