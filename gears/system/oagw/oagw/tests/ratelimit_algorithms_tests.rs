//! The two counter algorithms of `cpt-cf-oagw-algo-token-bucket` and
//! `cpt-cf-oagw-algo-sliding-window`.
//!
//! Covers the full-bucket initialization, the burst up to `burst.capacity`,
//! the refill and its capacity ceiling, the clock that moved backwards, a
//! `cost` above the capacity, the sliding window's boundary behaviour, and the
//! whole-second `Retry-After` both derive.

// @cpt-dod:cpt-cf-oagw-dod-rate-limit-tests:p1

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::time::{Duration, Instant};

use oagw::domain::ratelimit::{
    SlidingWindow, TokenBucket, sliding_window, token_bucket,
};
use oagw::domain::upstream::{Sustained, Window};

fn sustained(rate: u64, window: Window) -> Sustained {
    Sustained {
        rate,
        window: Some(window),
    }
}

fn at_offset(start: Instant, millis: u64) -> Instant {
    start + Duration::from_millis(millis)
}

#[test]
fn an_absent_bucket_starts_full() {
    // A bucket no request has touched yet is initialized at full capacity, so
    // the first burst is admitted up to `burst.capacity` (§1.5).
    let start = Instant::now();
    let bucket = TokenBucket::full(50, &sustained(10, Window::Second), start);
    assert_eq!(bucket.tokens(), 50);
    assert_eq!(bucket.capacity, 50);
}

#[test]
fn a_burst_is_admitted_up_to_the_capacity() {
    // A 50-capacity bucket charged 1 per request admits 50 requests and
    // refuses the 51st in the same instant.
    let start = Instant::now();
    let mut bucket = TokenBucket::full(50, &sustained(10, Window::Second), start);
    for _ in 0..50 {
        let outcome = token_bucket(&mut bucket, 1, start);
        assert!(outcome.admitted, "the burst is inside the capacity");
    }
    assert_eq!(bucket.tokens(), 0);
    let refused = token_bucket(&mut bucket, 1, start);
    assert!(!refused.admitted, "the bucket is empty");
    assert_eq!(refused.remaining, 0);
}

#[test]
fn a_cost_above_the_remaining_refuses_at_the_tenth_request() {
    // A 50-capacity bucket charged 10 per request admits 5 requests and
    // refuses the 6th, whose shortfall the bucket has not refilled yet.
    let start = Instant::now();
    let mut bucket = TokenBucket::full(50, &sustained(10, Window::Second), start);
    for index in 0..5 {
        let outcome = token_bucket(&mut bucket, 10, start);
        assert!(outcome.admitted);
        assert_eq!(outcome.remaining, 50 - 10 * (index + 1));
    }
    let refused = token_bucket(&mut bucket, 10, start);
    assert!(!refused.admitted);
    assert_eq!(refused.remaining, 0);
}

#[test]
fn the_refill_is_capped_at_the_capacity() {
    // A bucket left idle far longer than it takes to fill comes back at the
    // capacity and never above it.
    let start = Instant::now();
    let mut bucket = TokenBucket::full(50, &sustained(10, Window::Second), start);
    let outcome = token_bucket(&mut bucket, 50, start);
    assert!(outcome.admitted);
    bucket.refill(at_offset(start, 60_000));
    assert_eq!(bucket.tokens(), 50, "the refill stops at the capacity");
}

#[test]
fn the_refill_tracks_the_sustained_rate() {
    // A 50-capacity bucket at 10 per second refills 1 token every 100 ms: 250
    // ms of idle time returns 2 tokens.
    let start = Instant::now();
    let mut bucket = TokenBucket::full(50, &sustained(10, Window::Second), start);
    let _ = token_bucket(&mut bucket, 50, start);
    bucket.refill(at_offset(start, 250));
    assert_eq!(bucket.tokens(), 2);
}

#[test]
fn a_clock_that_moved_backwards_adds_and_removes_nothing() {
    // An `Instant` that reads earlier than the bucket's last update is no
    // elapsed time at all rather than a negative refill, so a clock adjustment
    // adds no tokens and removes none (§1.4).
    let start = Instant::now();
    let mut bucket = TokenBucket::full(50, &sustained(10, Window::Second), start);
    let _ = token_bucket(&mut bucket, 50, start);
    assert_eq!(bucket.tokens(), 0);
    bucket.refill(start - Duration::from_secs(60));
    assert_eq!(bucket.tokens(), 0, "the backward reading earned nothing");
    assert_eq!(bucket.tokens(), 0, "and removed nothing");
}

#[test]
fn a_cost_above_the_capacity_is_refused_with_a_computed_delay() {
    // A `cost` no refill within the window can cover is refused, and the
    // delay the refusal reports is the shortfall against the refill rate
    // rounded up to a whole second: 5 tokens short at 10 per second is 1
    // second, and 5 tokens short at 1 per second is 5 seconds.
    let start = Instant::now();
    let mut fast = TokenBucket::full(5, &sustained(10, Window::Second), start);
    let _ = token_bucket(&mut fast, 5, start);
    let refused = token_bucket(&mut fast, 5, start);
    assert!(!refused.admitted);
    assert_eq!(refused.delay_seconds, 1);

    let mut slow = TokenBucket::full(5, &sustained(1, Window::Second), start);
    let _ = token_bucket(&mut slow, 5, start);
    let refused = token_bucket(&mut slow, 5, start);
    assert!(!refused.admitted);
    assert_eq!(refused.delay_seconds, 5);
}

#[test]
fn the_sliding_window_refuses_across_the_boundary() {
    // A window of 3 per second records 3 charges and refuses the 4th at the
    // same instant; the charge a refusal records is none, so the window does
    // not extend itself against the requests it refuses.
    let start = Instant::now();
    let mut window = SlidingWindow::default();
    for _ in 0..3 {
        let outcome = sliding_window(
            &mut window,
            1,
            3,
            Duration::from_secs(1),
            start,
        );
        assert!(outcome.admitted);
    }
    assert_eq!(window.total(), 3);
    let refused = sliding_window(&mut window, 1, 3, Duration::from_secs(1), start);
    assert!(!refused.admitted);
    assert_eq!(window.total(), 3, "a refusal records no charge");

    // The same refusal repeated at any instant inside the window answers the
    // same total, which is what keeps a rejected client from pushing its own
    // admission further out.
    let refused = sliding_window(
        &mut window,
        1,
        3,
        Duration::from_secs(1),
        at_offset(start, 500),
    );
    assert!(!refused.admitted);
    assert_eq!(window.total(), 3);
    assert_eq!(refused.delay_seconds, 1);
}

#[test]
fn the_sliding_window_admits_at_the_boundary() {
    // A charge ages out the instant its window length has fully elapsed, so
    // the request at `start + 1s` is the admission the boundary permits.
    let start = Instant::now();
    let mut window = SlidingWindow::default();
    for _ in 0..3 {
        let _ = sliding_window(&mut window, 1, 3, Duration::from_secs(1), start);
    }
    let refused = sliding_window(&mut window, 1, 3, Duration::from_secs(1), start);
    assert!(!refused.admitted);

    let admitted = sliding_window(
        &mut window,
        1,
        3,
        Duration::from_secs(1),
        at_offset(start, 1_000),
    );
    assert!(admitted.admitted, "the boundary charge has aged out");
    assert_eq!(admitted.remaining, 2);
}

#[test]
fn the_sliding_window_reports_the_wait_for_the_oldest_charge() {
    // With the window full, the delay is the time until enough of the oldest
    // charges age out, rounded up to a whole second.
    let start = Instant::now();
    let mut window = SlidingWindow::default();
    for _ in 0..3 {
        let _ = sliding_window(&mut window, 1, 3, Duration::from_secs(1), start);
    }
    let refused = sliding_window(
        &mut window,
        1,
        3,
        Duration::from_secs(1),
        at_offset(start, 250),
    );
    assert!(!refused.admitted);
    assert_eq!(refused.delay_seconds, 1);
}

#[test]
fn a_cost_above_the_rate_is_never_admitted_by_the_window() {
    // A `cost` the window's rate cannot cover in one window is refused for as
    // long as the window holds any charge.
    let start = Instant::now();
    let mut window = SlidingWindow::default();
    let _ = sliding_window(&mut window, 1, 3, Duration::from_secs(1), start);
    let refused = sliding_window(&mut window, 4, 3, Duration::from_secs(1), start);
    assert!(!refused.admitted);
    assert_eq!(window.total(), 1);
}
