#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Unit tests for the rate limiter (ADR-0003).

use std::time::{Duration, Instant};

use uuid::Uuid;

use super::{Bucket, EffectiveLimit, RateLimiter, effective_limit};
use crate::domain::model::{BurstConfig, RateAlgorithm, RateLimitConfig, RateLimitScope, RateWindow, SustainedRate};

fn limit(rate: u32, window: Duration, capacity: u32) -> EffectiveLimit {
    EffectiveLimit {
        rate,
        window,
        capacity,
        algorithm: RateAlgorithm::TokenBucket,
        cost: 1,
        scope: RateLimitScope::Tenant,
    }
}

#[test]
fn merging_keeps_the_stricter_value_on_every_axis() {
    let generous = limit(100, Duration::from_secs(60), 100);
    let tight = EffectiveLimit {
        rate: 10,
        window: Duration::from_secs(30),
        capacity: 5,
        algorithm: RateAlgorithm::SlidingWindow,
        cost: 2,
        scope: RateLimitScope::Route,
    };
    let merged = EffectiveLimit::merge(Some(&generous), Some(&tight)).unwrap();
    assert_eq!(merged.rate, 10);
    assert_eq!(merged.window, Duration::from_secs(30));
    assert_eq!(merged.capacity, 5);
    assert_eq!(merged.algorithm, RateAlgorithm::SlidingWindow);
    assert_eq!(merged.cost, 2);
}

#[test]
fn merging_with_nothing_keeps_the_other_side() {
    let one = limit(1, Duration::from_secs(1), 1);
    assert_eq!(EffectiveLimit::merge(Some(&one), None), Some(one.clone()));
    assert_eq!(EffectiveLimit::merge(None, Some(&one)), Some(one));
    assert_eq!(EffectiveLimit::merge(None, None), None);
}

#[test]
fn a_global_scope_wins_over_a_narrower_one() {
    let global = EffectiveLimit {
        scope: RateLimitScope::Global,
        ..limit(10, Duration::from_secs(60), 10)
    };
    let local = EffectiveLimit {
        scope: RateLimitScope::Route,
        ..limit(10, Duration::from_secs(60), 10)
    };
    assert_eq!(
        EffectiveLimit::merge(Some(&global), Some(&local)).map(|l| l.scope),
        Some(RateLimitScope::Global)
    );
    assert_eq!(
        EffectiveLimit::merge(Some(&local), Some(&global)).map(|l| l.scope),
        Some(RateLimitScope::Global)
    );
}

#[test]
fn the_retry_interval_is_at_least_one_second_and_scaled_by_the_cost() {
    let cheap = limit(1, Duration::from_secs(1), 1);
    assert_eq!(cheap.retry_after_secs(), 1);
    let expensive = EffectiveLimit { cost: 5, ..cheap };
    assert_eq!(expensive.retry_after_secs(), 5);
    let slow = limit(1, Duration::from_secs(100), 1);
    assert_eq!(slow.retry_after_secs(), 100);
}

#[test]
fn a_token_bucket_refills_over_time() {
    let config = RateLimitConfig {
        sustained: SustainedRate { rate: 2, window: RateWindow::Second },
        burst: Some(BurstConfig { capacity: 2 }),
        ..RateLimitConfig::default()
    };
    let limit = effective_limit(&config);
    assert_eq!(limit.capacity, 2);
    let mut bucket = Bucket {
        tokens: 2.0,
        capacity: 2.0,
        refill_per_sec: 2.0,
        last: Instant::now(),
        window_hits: Vec::new(),
        algorithm: RateAlgorithm::TokenBucket,
    };
    let now = Instant::now();
    assert!(bucket.try_take(&limit, now));
    assert!(bucket.try_take(&limit, now));
    assert!(!bucket.try_take(&limit, now), "the bucket is empty");
    // One second later both tokens are back.
    let later = now + Duration::from_millis(1100);
    assert!(bucket.try_take(&limit, later));
}

#[test]
fn a_sliding_window_counts_requests_within_the_window() {
    let limit = EffectiveLimit {
        algorithm: RateAlgorithm::SlidingWindow,
        ..limit(2, Duration::from_secs(60), 2)
    };
    let mut bucket = Bucket {
        tokens: 2.0,
        capacity: 2.0,
        refill_per_sec: 0.0,
        last: Instant::now(),
        window_hits: Vec::new(),
        algorithm: RateAlgorithm::SlidingWindow,
    };
    let now = Instant::now();
    assert!(bucket.try_take(&limit, now));
    assert!(bucket.try_take(&limit, now));
    assert!(!bucket.try_take(&limit, now), "the window is full");
    // Half a window later nothing has been released yet.
    assert!(!bucket.try_take(&limit, now + Duration::from_secs(30)));
    // Once the window has rolled over the calls are allowed again.
    let later = now + Duration::from_secs(61);
    assert!(bucket.try_take(&limit, later));
}

#[test]
fn scope_keys_are_disjoint_per_sharing_mode() {
    let tenant = Uuid::from_u128(1);
    let route = Uuid::from_u128(2);
    let scoped = |scope| EffectiveLimit {
        scope,
        ..limit(10, Duration::from_secs(60), 10)
    };
    let key = |limit: &EffectiveLimit, subject: &str, ip: &str| {
        RateLimiter::scope_key(limit, tenant, route, subject, ip)
    };
    assert_eq!(key(&scoped(RateLimitScope::Global), "s", "1.2.3.4"), "global");
    assert_eq!(key(&scoped(RateLimitScope::Tenant), "s", "1.2.3.4"), "tenant:00000000-0000-0000-0000-000000000001");
    assert_eq!(key(&scoped(RateLimitScope::Route), "s", "1.2.3.4"), "route:00000000-0000-0000-0000-000000000002");
    assert_eq!(key(&scoped(RateLimitScope::User), "s", "1.2.3.4"), "user:00000000-0000-0000-0000-000000000001:s");
    assert_eq!(key(&scoped(RateLimitScope::Ip), "s", "1.2.3.4"), "ip:1.2.3.4");
}

#[tokio::test]
async fn the_limiter_rejects_the_call_that_exceeds_the_bucket() {
    let limiter = RateLimiter::new();
    let config = RateLimitConfig {
        sustained: SustainedRate { rate: 1, window: RateWindow::Second },
        burst: Some(BurstConfig { capacity: 1 }),
        ..RateLimitConfig::default()
    };
    let limit = effective_limit(&config);
    assert!(limiter.check("k", &limit).is_ok());
    assert!(limiter.check("k", &limit).is_err(), "the second call inside the window is refused");
    assert!(limiter.check("other", &limit).is_ok(), "a different key has its own bucket");
}

#[test]
fn the_effective_limit_falls_back_to_the_sustained_rate_without_a_burst() {
    let config = RateLimitConfig {
        sustained: SustainedRate { rate: 7, window: RateWindow::Minute },
        burst: None,
        ..RateLimitConfig::default()
    };
    let limit = effective_limit(&config);
    assert_eq!(limit.rate, 7);
    assert_eq!(limit.capacity, 7);
    assert_eq!(limit.window, Duration::from_secs(60));
}
