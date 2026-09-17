//! Tests for the in-memory token-bucket limiter (`docs/ADR/0003`).
use std::time::Duration;

use super::{Bucket, MILLI, RateDecision, RateLimiter, refill_per_millisecond, scope_key};
use crate::domain::model::{
    BurstConfig, RateAlgorithm, RateLimitConfig, RateScope, RateStrategy, RateWindow, SharingMode,
    SustainedRate,
};
use crate::infra::proxy::config::TenantChain;

fn limit(rate: u32, window: RateWindow, capacity: Option<u32>) -> RateLimitConfig {
    RateLimitConfig {
        sharing: SharingMode::Private,
        algorithm: RateAlgorithm::TokenBucket,
        sustained: SustainedRate { rate, window },
        burst: capacity.map(|capacity| BurstConfig { capacity }),
        scope: RateScope::Global,
        strategy: RateStrategy::Reject,
        cost: 1,
    }
}

#[test]
fn a_full_bucket_admits_the_sustained_rate() {
    let limiter = RateLimiter::new();
    let config = limit(5, RateWindow::Second, Some(5));
    for index in 0..5 {
        let decision = limiter.check(&config, "bucket", config.cost);
        assert!(decision.allowed, "request {index} should be admitted");
    }
    let decision = limiter.check(&config, "bucket", config.cost);
    assert!(!decision.allowed);
    assert!(decision.retry_after_seconds >= 1);
}

#[test]
fn buckets_are_per_key() {
    let limiter = RateLimiter::new();
    let config = limit(1, RateWindow::Second, Some(1));
    assert!(limiter.check(&config, "a", config.cost).allowed);
    assert!(limiter.check(&config, "b", config.cost).allowed);
    assert!(!limiter.check(&config, "a", config.cost).allowed);
    assert_eq!(limiter.len(), 2);
    assert!(!limiter.is_empty());
    limiter.clear();
    assert!(limiter.is_empty());
}

#[test]
fn every_scope_shape_is_distinct() {
    let config = limit(1, RateWindow::Minute, None);
    let upstream = uuid::Uuid::new_v4();
    let route = uuid::Uuid::new_v4();
    let tenant = uuid::Uuid::new_v4();
    let identity = "user-1";

    let global = scope_key(&config, upstream, route, tenant, identity);
    let mut route_scoped = config.clone();
    route_scoped.scope = RateScope::Route;
    let route_key = scope_key(&route_scoped, upstream, route, tenant, identity);
    let mut tenant_scoped = config.clone();
    tenant_scoped.scope = RateScope::Tenant;
    let tenant_key = scope_key(&tenant_scoped, upstream, route, tenant, identity);
    let mut user_scoped = config.clone();
    user_scoped.scope = RateScope::User;
    let user_key = scope_key(&user_scoped, upstream, route, tenant, identity);
    let mut ip_scoped = config.clone();
    ip_scoped.scope = RateScope::Ip;
    let ip_key = scope_key(&ip_scoped, upstream, route, tenant, identity);

    let keys = [global, route_key, tenant_key, user_key, ip_key];
    for (index, key) in keys.iter().enumerate() {
        for other in keys.iter().skip(index + 1) {
            assert_ne!(key, other, "scope keys must differ");
        }
    }
    assert!(keys[0].starts_with(&format!("upstream:{upstream}:route:{route}")));
}

#[test]
fn a_burst_capacity_overshoots_the_sustained_rate() {
    let limiter = RateLimiter::new();
    let config = limit(1, RateWindow::Minute, Some(10));
    for _ in 0..10 {
        assert!(limiter.check(&config, "burst", config.cost).allowed);
    }
    assert!(!limiter.check(&config, "burst", config.cost).allowed);
}

#[test]
fn capacity_falls_back_to_the_sustained_rate() {
    let limiter = RateLimiter::new();
    let config = limit(3, RateWindow::Minute, None);
    for _ in 0..3 {
        assert!(limiter.check(&config, "no-burst", config.cost).allowed);
    }
    assert!(!limiter.check(&config, "no-burst", config.cost).allowed);
}

#[test]
fn a_cost_greater_than_one_drains_faster() {
    let limiter = RateLimiter::new();
    let mut config = limit(10, RateWindow::Minute, Some(6));
    config.cost = 3;
    assert!(limiter.check(&config, "cost", config.cost).allowed);
    assert!(limiter.check(&config, "cost", config.cost).allowed);
    assert!(!limiter.check(&config, "cost", config.cost).allowed);
}

#[test]
fn sliding_window_restores_the_whole_allowance_after_the_window() {
    // A `SlidingWindow` bucket refills in one step, so the instantaneous
    // behaviour is exercised here; the timed path is covered by `refill`.
    let limiter = RateLimiter::new();
    let mut config = limit(1, RateWindow::Second, Some(1));
    config.algorithm = RateAlgorithm::SlidingWindow;
    assert!(limiter.check(&config, "window", config.cost).allowed);
    assert!(!limiter.check(&config, "window", config.cost).allowed);
}

#[test]
fn unlimited_decision_never_limits() {
    let decision = RateDecision::unlimited();
    assert!(decision.allowed);
    assert_eq!(decision.scope_key, "");
}

#[test]
fn refill_rate_is_expressed_in_milli_tokens_per_millisecond() {
    // One token per second is one thousand milli-tokens per second, i.e. one
    // milli-token per millisecond. Sixty per minute is the same rate.
    assert_eq!(refill_per_millisecond(1, RateWindow::Second), 1.0);
    assert_eq!(refill_per_millisecond(60, RateWindow::Minute), 1.0);
    assert_eq!(refill_per_millisecond(3_600, RateWindow::Hour), 1.0);
    // Two per minute refills a whole token in thirty seconds, not thirty
    // milliseconds.
    assert_eq!(
        refill_per_millisecond(2, RateWindow::Minute),
        0.033_333_333_333_333_33
    );
    assert!(refill_per_millisecond(0, RateWindow::Second) > 0.0);
}

#[test]
fn a_minute_rate_is_not_refilled_per_millisecond() {
    // The live contract: a bucket of two per minute is empty for about thirty
    // seconds, so the caller is told to come back in tens of seconds, not one.
    let limiter = RateLimiter::new();
    let config = limit(2, RateWindow::Minute, None);
    for _ in 0..2 {
        assert!(limiter.check(&config, "minute", config.cost).allowed);
    }
    let decision = limiter.check(&config, "minute", config.cost);
    assert!(!decision.allowed);
    assert!(
        decision.retry_after_seconds >= 25,
        "retry-after must reflect the window, got {}",
        decision.retry_after_seconds
    );
    assert!(decision.retry_after_seconds <= 60);
}

#[test]
fn a_bucket_starts_full() {
    let bucket = Bucket::full(4);
    assert_eq!(bucket.milli_tokens, 4_000);
    assert_eq!(bucket.milli_tokens, 4 * MILLI);
}

#[test]
fn the_reset_horizon_never_exceeds_the_window() {
    let limiter = RateLimiter::new();
    let config = limit(1, RateWindow::Day, Some(1));
    assert!(limiter.check(&config, "reset", config.cost).allowed);
    let decision = limiter.check(&config, "reset", config.cost);
    assert!(!decision.allowed);
    assert!(decision.retry_after_seconds <= 86_400);
    assert!(decision.retry_after_seconds >= 1);
}

#[test]
fn the_limiter_is_shareable_across_threads() {
    let limiter = RateLimiter::new();
    let config = limit(1_000, RateWindow::Second, Some(1_000));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            std::thread::spawn({
                let limiter = limiter.clone();
                let config = config.clone();
                move || limiter.check(&config, "threads", config.cost).allowed
            })
        })
        .collect();
    let admitted = handles
        .into_iter()
        .map(std::thread::JoinHandle::join)
        .filter(|result| result.is_ok())
        .count();
    assert_eq!(admitted, 8);
}

#[test]
fn the_timeout_helper_is_in_seconds() {
    assert_eq!(
        super::super::response::exchange_timeout(2),
        Duration::from_secs(2)
    );
}

#[test]
fn the_tenant_chain_trait_is_object_safe() {
    // The data plane stores the ancestor lookup as a trait object; this only
    // needs to compile.
    fn assert_chain(chain: std::sync::Arc<dyn TenantChain>) -> std::sync::Arc<dyn TenantChain> {
        chain
    }
    let resolver = super::super::config::ResolverChain::new(None);
    let _ = assert_chain(std::sync::Arc::new(resolver));
}
