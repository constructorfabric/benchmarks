//! `RateLimiter` tests (`ADR`-0003).
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::time::Duration;

use serde_json::json;

use crate::domain::error::DomainError;
use crate::domain::model::{
    Burst, RateLimitConfig, RateScope, RateStrategy, RateWindow, SustainedRate,
};
use crate::infra::proxy::ratelimit::RateLimiter;

fn config(json: serde_json::Value) -> RateLimitConfig {
    serde_json::from_value(json).unwrap()
}

fn limiter(json: serde_json::Value) -> RateLimiter {
    RateLimiter::new(&config(json))
}

#[test]
fn the_default_bucket_holds_one_window_of_sustained_traffic() {
    let config = config(json!({"sustained": {"rate": 10, "window": "second"}}));
    let limiter = RateLimiter::new(&config);
    assert_eq!(limiter.limit(), 10);
}

#[test]
fn burst_capacity_sizes_the_bucket() {
    let config = config(json!({
        "sustained": {"rate": 10, "window": "second"},
        "burst": {"capacity": 100}
    }));
    let limiter = RateLimiter::new(&config);
    assert_eq!(limiter.limit(), 100);
}

#[test]
fn a_config_that_omits_the_burst_defaults_it_to_the_sustained_rate() {
    let config = config(json!({"sustained": {"rate": 5}}));
    let limiter = RateLimiter::new(&config);
    assert_eq!(limiter.limit(), 5);
}

#[test]
fn a_bucket_never_shrinks_below_one_window_of_sustained_traffic() {
    let config = config(json!({
        "sustained": {"rate": 20, "window": "second"},
        "burst": {"capacity": 5}
    }));
    let limiter = RateLimiter::new(&config);
    assert_eq!(limiter.limit(), 20);
}

#[test]
fn the_configured_cost_is_spent_per_request() {
    let config = config(json!({"sustained": {"rate": 10}, "cost": 4}));
    let limiter = RateLimiter::new(&config);
    assert_eq!(limiter.cost(), 4);
}

#[tokio::test]
async fn a_request_reports_what_is_left_and_when_the_bucket_is_full() {
    let config = config(json!({
        "sustained": {"rate": 6, "window": "minute"},
        "burst": {"capacity": 6},
        "cost": 2
    }));
    let limiter = RateLimiter::new(&config);
    let key = limiter.key(uuid::Uuid::nil(), uuid::Uuid::nil(), None, None);

    let first = limiter.check(&key).unwrap();
    assert_eq!(first.limit, 6);
    assert_eq!(first.remaining, 4);

    let second = limiter.check(&key).unwrap();
    assert_eq!(second.remaining, 2);

    let reset = second.reset;
    assert!(reset > 0, "a part-spent bucket names its refill");
    assert!(reset <= 60, "a minute window refills within a minute");
}

#[tokio::test]
async fn exhausting_the_bucket_is_a_429_with_retry_after() {
    let config = config(json!({"sustained": {"rate": 1, "window": "minute"}}));
    let limiter = RateLimiter::new(&config);
    let key = limiter.key(uuid::Uuid::nil(), uuid::Uuid::nil(), None, None);
    limiter.check(&key).unwrap();
    let error = limiter.check(&key).unwrap_err();
    let DomainError::RateLimitExceeded { retry_after, .. } = error else {
        panic!("expected RateLimitExceeded");
    };
    let retry_after = retry_after.expect("a rejected bucket names its wait");
    assert!(retry_after > Duration::from_secs(0));
    assert!(
        retry_after <= Duration::from_mins(2),
        "a one-token bucket refills soon"
    );
}

#[tokio::test]
async fn counters_of_different_tenants_are_independent() {
    let config = config(json!({"sustained": {"rate": 1, "window": "minute"}}));
    let limiter = RateLimiter::new(&config);
    let first = limiter.key(uuid::Uuid::now_v7(), uuid::Uuid::nil(), None, None);
    let second = limiter.key(uuid::Uuid::now_v7(), uuid::Uuid::nil(), None, None);
    limiter.check(&first).unwrap();
    limiter.check(&first).unwrap_err();
    limiter.check(&second).unwrap();
}

#[tokio::test]
async fn the_scope_selects_the_counter_key() {
    let tenant = uuid::Uuid::now_v7();
    let subject = uuid::Uuid::now_v7();
    let route = uuid::Uuid::now_v7();

    assert_eq!(
        scoped("global").key(tenant, subject, Some("1.2.3.4"), Some(route)),
        "global"
    );
    assert_eq!(
        scoped("tenant").key(tenant, subject, None, None),
        format!("tenant:{tenant}")
    );
    assert_eq!(
        scoped("user").key(tenant, subject, None, None),
        format!("user:{tenant}:{subject}")
    );
    assert_eq!(
        scoped("ip").key(tenant, subject, Some("10.0.0.1"), None),
        "ip:10.0.0.1"
    );
    assert_eq!(scoped("ip").key(tenant, subject, None, None), "ip:unknown");
    assert!(
        scoped("route")
            .key(tenant, subject, None, Some(route))
            .contains(&route.to_string())
    );
}

fn scoped(scope: &str) -> RateLimiter {
    limiter(json!({"sustained": {"rate": 1}, "scope": scope}))
}

#[test]
fn a_sustained_rate_of_one_per_minute_never_refills_within_a_test_step() {
    let config = RateLimitConfig {
        sharing: crate::domain::model::SharingMode::Private,
        algorithm: crate::domain::model::RateAlgorithm::TokenBucket,
        sustained: SustainedRate {
            rate: 1,
            window: RateWindow::Minute,
        },
        burst: Some(Burst { capacity: 1 }),
        scope: RateScope::Tenant,
        strategy: RateStrategy::Reject,
        cost: 1,
    };
    let limiter = RateLimiter::new(&config);
    let key = limiter.key(uuid::Uuid::nil(), uuid::Uuid::nil(), None, None);
    assert!(limiter.check(&key).is_ok());
    assert!(limiter.check(&key).is_err());
}

#[test]
fn an_hourly_window_refills_slowly() {
    let config = config(json!({"sustained": {"rate": 3600, "window": "hour"}}));
    let limiter = RateLimiter::new(&config);
    let key = limiter.key(uuid::Uuid::nil(), uuid::Uuid::nil(), None, None);
    for _ in 0..100 {
        limiter.check(&key).unwrap();
    }
}
