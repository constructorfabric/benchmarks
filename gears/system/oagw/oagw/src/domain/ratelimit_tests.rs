//! Unit tests for the token bucket.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::domain::model::{
    Burst, RateAlgorithm, RateScope, RateStrategy, RateWindow, SharingMode, SustainedRate,
};
use std::time::Duration;

fn limit(rate: u32, capacity: Option<u32>, window: RateWindow) -> RateLimit {
    RateLimit {
        sharing: SharingMode::Private,
        algorithm: RateAlgorithm::TokenBucket,
        sustained: SustainedRate { rate, window },
        burst: Burst { capacity },
        scope: RateScope::Tenant,
        strategy: RateStrategy::Reject,
        cost: 1,
    }
}

fn key(name: &str) -> BucketKey {
    BucketKey::new("tenant", "upstream", name)
}

#[test]
fn burst_allows_up_to_capacity() {
    let config = limit(10, Some(3), RateWindow::Second);
    let limiter = RateLimiter::new();
    let start = Instant::now();
    for _ in 0..3 {
        assert!(limiter.check(&key("a"), &config, 1, start).allowed);
    }
    assert!(!limiter.check(&key("a"), &config, 1, start).allowed);
}

#[test]
fn capacity_defaults_to_the_sustained_rate() {
    let config = limit(7, None, RateWindow::Second);
    let limiter = RateLimiter::new();
    let start = Instant::now();
    let decision = limiter.check(&key("a"), &config, 1, start);
    assert_eq!(decision.limit, 7);
    for _ in 0..6 {
        assert!(
            limiter.check(&key("a"), &config, 1, start).allowed,
            "the bucket still has tokens"
        );
    }
    assert!(!limiter.check(&key("a"), &config, 1, start).allowed);
}

#[test]
fn buckets_refill_over_time() {
    let config = limit(10, Some(10), RateWindow::Second);
    let limiter = RateLimiter::new();
    let start = Instant::now();
    for _ in 0..10 {
        assert!(limiter.check(&key("a"), &config, 1, start).allowed);
    }
    assert!(!limiter.check(&key("a"), &config, 1, start).allowed);
    // Half a second at 10/second restores five tokens.
    let later = start + Duration::from_millis(500);
    assert!(limiter.check(&key("a"), &config, 1, later).allowed);
}

#[test]
fn cost_weights_the_consumption() {
    let config = limit(10, Some(4), RateWindow::Second);
    let mut weighted = config;
    weighted.cost = 3;
    let limiter = RateLimiter::new();
    let start = Instant::now();
    assert!(
        limiter
            .check(&key("a"), &weighted, weighted.cost, start)
            .allowed
    );
    assert!(
        !limiter
            .check(&key("a"), &weighted, weighted.cost, start)
            .allowed
    );
    let _ = config;
}

#[test]
fn scopes_are_keyed_independently() {
    let config = limit(1, Some(1), RateWindow::Second);
    let limiter = RateLimiter::new();
    let start = Instant::now();
    assert!(
        limiter
            .check(&BucketKey::new("tenant", "u", "p"), &config, 1, start)
            .allowed
    );
    assert!(
        limiter
            .check(&BucketKey::new("route", "u", "p"), &config, 1, start)
            .allowed
    );
    assert!(
        limiter
            .check(&BucketKey::new("tenant", "v", "p"), &config, 1, start)
            .allowed
    );
    assert!(
        limiter
            .check(&BucketKey::new("tenant", "u", "q"), &config, 1, start)
            .allowed
    );
    assert!(
        !limiter
            .check(&BucketKey::new("tenant", "u", "p"), &config, 1, start)
            .allowed
    );
}

#[test]
fn retry_after_is_computed_from_the_refill_rate() {
    let config = limit(2, Some(2), RateWindow::Second);
    let limiter = RateLimiter::new();
    let start = Instant::now();
    assert!(limiter.check(&key("a"), &config, 1, start).allowed);
    assert!(limiter.check(&key("a"), &config, 1, start).allowed);
    let decision = limiter.check(&key("a"), &config, 1, start);
    assert!(!decision.allowed);
    assert!(decision.reset_seconds >= 1);

    let error = RateLimiter::exceeded(&decision);
    assert_eq!(error.status(), 429);
    assert!(error.retry_after().is_some_and(|d| d.as_secs() >= 1));
}

#[test]
fn headers_carry_the_triple() {
    let decision = RateDecision {
        allowed: true,
        limit: 10,
        remaining: 7,
        reset_seconds: 3,
    };
    let headers = rate_limit_headers(&decision);
    assert_eq!(headers[0].0, "X-RateLimit-Limit");
    assert_eq!(headers[0].1, "10");
    assert_eq!(headers[1].1, "7");
    assert_eq!(headers[2].1, "3");
}

#[test]
fn cross_window_refill_matches_the_window() {
    // 120 per minute == 2 per second.
    let config = limit(120, Some(120), RateWindow::Minute);
    let limiter = RateLimiter::new();
    let start = Instant::now();
    for _ in 0..120 {
        assert!(limiter.check(&key("a"), &config, 1, start).allowed);
    }
    let later = start + Duration::from_secs(1);
    assert!(limiter.check(&key("a"), &config, 1, later).allowed);
}
