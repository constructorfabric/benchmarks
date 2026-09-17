// Created: 2026-09-04 by Constructor Tech
//! Tests of the token-bucket rate limiter of the data plane.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::num::NonZeroU32;
use std::time::Duration;

use uuid::Uuid;

use super::*;
use crate::domain::{
    BurstCapacity, RateLimitAlgorithm, RateLimitConfig, RateLimitScope, RateLimitStrategy,
    RateLimitWindow, SharingMode, SustainedRate,
};

/// The identity shared by the tests of this module.
fn identity() -> ScopeIdentity {
    ScopeIdentity {
        tenant_id: Uuid::from_u128(0x0A6D),
        subject_id: Uuid::from_u128(0x5EB),
        client_ip: Some(String::from("203.0.113.7")),
    }
}

/// `rate` tokens per second with a burst capacity of `capacity`.
fn limit(rate: u32, capacity: u32, scope: RateLimitScope) -> RateLimitConfig {
    RateLimitConfig {
        sharing: SharingMode::Inherit,
        algorithm: RateLimitAlgorithm::TokenBucket,
        sustained: SustainedRate {
            rate: NonZeroU32::new(rate).unwrap(),
            window: RateLimitWindow::Second,
        },
        burst: Some(BurstCapacity {
            capacity: NonZeroU32::new(capacity).unwrap(),
        }),
        scope,
        strategy: RateLimitStrategy::Reject,
        cost: NonZeroU32::new(1).unwrap(),
    }
}

/// Upstream identifier of the tests.
fn upstream_id() -> Uuid {
    Uuid::from_u128(0x1)
}

#[test]
fn bucket_allows_the_capacity_then_rejects() {
    let limiter = RateLimiter::new();
    let config = limit(2, 2, RateLimitScope::Ip);
    let key = bucket_key(&config, upstream_id(), None, &identity());
    let now = Instant::now();

    let first = limiter.check(&key, &config, now);
    assert!(first.allowed);
    assert_eq!(first.limit, 2);
    assert_eq!(first.remaining, 1);
    assert_eq!(first.retry_after_secs, 0);

    let second = limiter.check(&key, &config, now);
    assert!(second.allowed);
    assert_eq!(second.remaining, 0);

    let third = limiter.check(&key, &config, now);
    assert!(!third.allowed);
    assert_eq!(third.remaining, 0);
    assert!(third.retry_after_secs >= 1, "the retry advice is provided");
}

#[test]
fn rejected_decision_carries_the_rate_limit_headers() {
    let limiter = RateLimiter::new();
    let config = limit(1, 1, RateLimitScope::Ip);
    let key = bucket_key(&config, upstream_id(), None, &identity());
    let now = Instant::now();

    assert!(limiter.check(&key, &config, now).allowed);
    let rejected = limiter.check(&key, &config, now);
    assert!(!rejected.allowed);

    let mut headers = http::HeaderMap::new();
    rejected.write_headers(&mut headers);
    for name in [
        RATE_LIMIT_LIMIT_HEADER,
        RATE_LIMIT_REMAINING_HEADER,
        RATE_LIMIT_RESET_HEADER,
        LEGACY_LIMIT_HEADER,
        LEGACY_REMAINING_HEADER,
        LEGACY_RESET_HEADER,
    ] {
        assert!(
            headers.contains_key(name),
            "{name} is part of the accounting"
        );
    }
    assert_eq!(headers.get(RATE_LIMIT_LIMIT_HEADER).unwrap(), "1");
    assert_eq!(headers.get(RATE_LIMIT_REMAINING_HEADER).unwrap(), "0");

    let error = rejected.error();
    match error {
        OagwError::RateLimitExceeded {
            retry_after_secs,
            detail,
        } => {
            assert_eq!(retry_after_secs, rejected.retry_after_secs);
            assert!(detail.contains("rate limit"));
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn bucket_recovers_after_the_refill_interval() {
    let limiter = RateLimiter::new();
    let config = limit(2, 2, RateLimitScope::Ip);
    let key = bucket_key(&config, upstream_id(), None, &identity());
    let start = Instant::now();

    assert!(limiter.check(&key, &config, start).allowed);
    assert!(limiter.check(&key, &config, start).allowed);
    assert!(!limiter.check(&key, &config, start).allowed);

    // One second later the sustained rate has replenished two tokens.
    let later = start + Duration::from_millis(1_100);
    let recovered = limiter.check(&key, &config, later);
    assert!(
        recovered.allowed,
        "the bucket refills from the sustained rate"
    );
    assert_eq!(recovered.remaining, 1);
}

#[test]
fn buckets_are_scoped_by_the_configuration_key() {
    let limiter = RateLimiter::new();
    let config = limit(1, 1, RateLimitScope::Ip);
    let now = Instant::now();

    let tenant_key = bucket_key(&config, upstream_id(), None, &identity());
    assert!(limiter.check(&tenant_key, &config, now).allowed);

    let mut other = identity();
    other.client_ip = Some(String::from("198.51.100.9"));
    let other_key = bucket_key(&config, upstream_id(), None, &other);
    assert_ne!(tenant_key, other_key);
    assert!(limiter.check(&other_key, &config, now).allowed);
}

#[test]
fn bucket_keys_separate_scopes_and_resources() {
    let ip = limit(1, 1, RateLimitScope::Ip);
    let user = limit(1, 1, RateLimitScope::User);
    let tenant = limit(1, 1, RateLimitScope::Tenant);
    let global = limit(1, 1, RateLimitScope::Global);
    let route = limit(1, 1, RateLimitScope::Route);
    let route_id = Some(Uuid::from_u128(0x2));

    let tenant_scope = bucket_key(&tenant, upstream_id(), route_id, &identity());
    let route_scope = bucket_key(&route, upstream_id(), route_id, &identity());
    let ip_scope = bucket_key(&ip, upstream_id(), route_id, &identity());
    let user_scope = bucket_key(&user, upstream_id(), route_id, &identity());
    let global_scope = bucket_key(&global, upstream_id(), route_id, &identity());

    let keys = [
        &tenant_scope,
        &route_scope,
        &ip_scope,
        &user_scope,
        &global_scope,
    ];
    for (index, key) in keys.iter().enumerate() {
        for other in &keys[index + 1..] {
            assert_ne!(key, other, "{key} and {other} must differ");
        }
    }
    assert!(tenant_scope.starts_with("oagw:ratelimit:"));
    assert!(tenant_scope.contains("upstream:"));
    assert!(route_scope.contains("route:"));
}

#[test]
fn ip_scope_falls_back_to_the_tenant_when_the_ip_is_unknown() {
    let config = limit(1, 1, RateLimitScope::Ip);
    let mut anonymous = identity();
    anonymous.client_ip = None;
    let key = bucket_key(&config, Uuid::from_u128(0x3), None, &anonymous);
    assert!(key.ends_with(anonymous.tenant_id.to_string().as_str()));
}

#[test]
fn configuration_changes_reset_the_bucket() {
    let limiter = RateLimiter::new();
    let strict = limit(1, 1, RateLimitScope::Ip);
    let loose = limit(4, 4, RateLimitScope::Ip);
    let key = bucket_key(&strict, upstream_id(), None, &identity());
    let now = Instant::now();

    assert!(limiter.check(&key, &strict, now).allowed);
    assert!(!limiter.check(&key, &strict, now).allowed);
    // A new capacity replaces the exhausted bucket instead of inheriting it.
    let refreshed = limiter.check(&key, &loose, now);
    assert!(refreshed.allowed);
    assert_eq!(refreshed.limit, 4);
}

#[test]
fn sliding_window_has_no_burst_allowance() {
    let limiter = RateLimiter::new();
    let mut config = limit(2, 8, RateLimitScope::Ip);
    config.algorithm = RateLimitAlgorithm::SlidingWindow;
    let key = bucket_key(&config, upstream_id(), None, &identity());
    let now = Instant::now();

    assert!(limiter.check(&key, &config, now).allowed);
    assert!(limiter.check(&key, &config, now).allowed);
    assert!(!limiter.check(&key, &config, now).allowed);
}

#[test]
fn cost_of_the_request_is_charged() {
    let limiter = RateLimiter::new();
    let mut config = limit(2, 2, RateLimitScope::Ip);
    config.cost = NonZeroU32::new(2).unwrap();
    let key = bucket_key(&config, upstream_id(), None, &identity());
    let now = Instant::now();

    let first = limiter.check(&key, &config, now);
    assert!(first.allowed);
    assert_eq!(first.remaining, 0);
    assert!(!limiter.check(&key, &config, now).allowed);
}

#[test]
fn effective_limit_is_the_strictest_of_the_hierarchy() {
    let upstream = limit(10, 10, RateLimitScope::Tenant);
    let mut route = limit(5, 3, RateLimitScope::User);

    // A private upstream limit is invisible: the route value applies alone.
    let mut private = upstream.clone();
    private.sharing = SharingMode::Private;
    let effective = effective_limit(Some(&private), Some(&route));
    assert_eq!(
        effective.as_ref().map(|limit| limit.scope),
        Some(RateLimitScope::User)
    );

    // An enforced upstream limit is combined through `min()`.
    let mut enforced = upstream.clone();
    enforced.sharing = SharingMode::Enforce;
    let effective = effective_limit(Some(&enforced), Some(&route)).unwrap();
    assert_eq!(effective.sustained.rate.get(), 5);
    assert_eq!(effective.burst.map(|burst| burst.capacity.get()), Some(3));
    // The descendant owns the scope, the algorithm and the strategy.
    assert_eq!(effective.scope, RateLimitScope::User);
    assert_eq!(effective.algorithm, RateLimitAlgorithm::TokenBucket);

    // Without a route limit the upstream limit applies as-is.
    let effective = effective_limit(Some(&enforced), None).unwrap();
    assert_eq!(effective.sustained.rate.get(), 10);

    // Without an upstream limit the route limit applies as-is.
    let effective = effective_limit(None, Some(&route)).unwrap();
    assert_eq!(effective.sustained.rate.get(), 5);

    // A route limit alone is still honoured when the ancestor is private.
    route.burst = None;
    let effective = effective_limit(Some(&private), Some(&route)).unwrap();
    assert_eq!(effective.burst, None);
}

#[test]
fn rejects_only_the_rejecting_strategies() {
    let mut config = limit(1, 1, RateLimitScope::Ip);
    config.strategy = RateLimitStrategy::Reject;
    assert!(rejects(&config));
    config.strategy = RateLimitStrategy::Queue;
    assert!(rejects(&config));
    config.strategy = RateLimitStrategy::Degrade;
    assert!(!rejects(&config));
}

#[test]
fn clear_drops_every_bucket() {
    let limiter = RateLimiter::new();
    let config = limit(1, 1, RateLimitScope::Ip);
    let key = bucket_key(&config, upstream_id(), None, &identity());
    let now = Instant::now();

    assert!(limiter.check(&key, &config, now).allowed);
    assert!(!limiter.check(&key, &config, now).allowed);
    limiter.clear();
    assert!(limiter.check(&key, &config, now).allowed);
}
