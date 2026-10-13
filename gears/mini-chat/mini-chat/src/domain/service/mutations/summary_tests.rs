#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The summary coverage comparison at, before and after the frontier, in the
//! order `(created_at ASC, id ASC)`.

use time::OffsetDateTime;
use uuid::Uuid;

use super::summary_covers;
use crate::domain::service::stream::ports::MessageBoundary;

fn boundary(secs: i64, id: u128) -> MessageBoundary {
    MessageBoundary {
        created_at: OffsetDateTime::from_unix_timestamp(1_800_000_000 + secs).unwrap(),
        id: Uuid::from_u128(id),
    }
}

#[test]
fn a_frontier_after_the_user_message_covers_it() {
    assert!(summary_covers(boundary(20, 1), boundary(10, 9)));
}

#[test]
fn a_frontier_exactly_at_the_user_message_covers_it() {
    assert!(summary_covers(boundary(10, 5), boundary(10, 5)));
}

#[test]
fn a_frontier_before_the_user_message_does_not_cover_it() {
    assert!(!summary_covers(boundary(5, 9), boundary(10, 1)));
}

#[test]
fn at_the_same_time_the_greater_id_covers() {
    assert!(summary_covers(boundary(10, 6), boundary(10, 5)));
}

#[test]
fn at_the_same_time_the_smaller_id_does_not_cover() {
    assert!(!summary_covers(boundary(10, 4), boundary(10, 5)));
}

#[test]
fn a_frontier_one_microsecond_before_does_not_cover() {
    let user_message = boundary(10, 5);
    let frontier = MessageBoundary {
        created_at: user_message.created_at - time::Duration::microseconds(1),
        id: Uuid::from_u128(u128::MAX),
    };
    assert!(!summary_covers(frontier, user_message));
}
