#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The mutation time rule: the clock time, raised to 1 microsecond after the
//! later of the replaced turn's `started_at` and its user message
//! `created_at` when the clock is not past them.

use time::OffsetDateTime;

use super::mutation_time;

fn at(micros: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap()
        + time::Duration::microseconds(micros)
}

#[test]
fn a_clock_past_both_times_is_used_as_it_is() {
    assert_eq!(mutation_time(at(1_000), at(10), at(20)), at(1_000));
}

#[test]
fn a_clock_one_microsecond_past_the_later_time_is_used_as_it_is() {
    assert_eq!(mutation_time(at(21), at(10), at(20)), at(21));
}

#[test]
fn a_clock_equal_to_the_turn_start_is_raised_by_one_microsecond() {
    assert_eq!(mutation_time(at(500), at(500), at(100)), at(501));
}

#[test]
fn a_clock_equal_to_the_user_message_time_is_raised_by_one_microsecond() {
    assert_eq!(mutation_time(at(500), at(100), at(500)), at(501));
}

#[test]
fn a_clock_behind_the_turn_start_gives_the_start_plus_one_microsecond() {
    // Clock skew between pods: the replacement must still sort after the turn.
    assert_eq!(mutation_time(at(0), at(900), at(100)), at(901));
}

#[test]
fn a_clock_behind_the_user_message_gives_the_message_time_plus_one_microsecond() {
    assert_eq!(mutation_time(at(0), at(100), at(900)), at(901));
}

#[test]
fn the_later_of_the_two_times_decides() {
    assert_eq!(mutation_time(at(0), at(300), at(200)), at(301));
    assert_eq!(mutation_time(at(0), at(200), at(300)), at(301));
}
