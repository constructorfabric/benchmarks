//! Token-bucket behaviour and counter scoping.

use super::*;
use crate::domain::model::{BurstCapacity, RateWindow, SharingMode, SustainedRate};

fn config(rate: u32, capacity: Option<u32>) -> RateLimitConfig {
    RateLimitConfig {
        sharing: SharingMode::Private,
        algorithm: RateLimitAlgorithm::TokenBucket,
        sustained: SustainedRate {
            rate,
            window: RateWindow::Second,
        },
        burst: BurstCapacity { capacity },
        budget: None,
        scope: crate::domain::model::RateLimitScope::Tenant,
        strategy: crate::domain::model::RateLimitStrategy::Reject,
        cost: 1,
        response_headers: true,
    }
}

fn subject() -> RateLimitSubject {
    RateLimitSubject {
        tenant_id: Uuid::from_u128(1),
        subject_id: Uuid::from_u128(2),
        upstream_id: Uuid::from_u128(3),
        route_id: Uuid::from_u128(4),
    }
}

#[test]
fn a_burst_is_allowed_up_to_capacity_then_refused() {
    let registry = RateLimiterRegistry::new();
    let config = config(1, Some(3));
    let subject = subject();
    let now = Instant::now();

    for attempt in 0..3 {
        let verdict = registry.check_at(&config, &subject, None, now);
        assert!(verdict.allowed, "attempt {attempt} should be inside the burst");
    }
    let verdict = registry.check_at(&config, &subject, None, now);
    assert!(!verdict.allowed);
    assert_eq!(verdict.remaining, 0);
    assert!(verdict.retry_after_secs >= 1);
    assert!(verdict.usage_ratio > 0.99);
}

#[test]
fn tokens_replenish_with_elapsed_time() {
    let registry = RateLimiterRegistry::new();
    let config = config(2, Some(2));
    let subject = subject();
    let start = Instant::now();

    assert!(registry.check_at(&config, &subject, None, start).allowed);
    assert!(registry.check_at(&config, &subject, None, start).allowed);
    assert!(!registry.check_at(&config, &subject, None, start).allowed);

    // Two tokens per second: one second later the bucket is full again.
    let later = start + Duration::from_secs(1);
    assert!(registry.check_at(&config, &subject, None, later).allowed);
    assert!(registry.check_at(&config, &subject, None, later).allowed);
}

#[test]
fn cost_consumes_more_than_one_token() {
    let registry = RateLimiterRegistry::new();
    let mut config = config(10, Some(10));
    config.cost = 4;
    let subject = subject();
    let now = Instant::now();

    assert!(registry.check_at(&config, &subject, None, now).allowed);
    assert!(registry.check_at(&config, &subject, None, now).allowed);
    // 8 of 10 consumed; a third request of cost 4 does not fit.
    assert!(!registry.check_at(&config, &subject, None, now).allowed);
}

#[test]
fn a_sliding_window_ignores_the_burst_capacity() {
    let registry = RateLimiterRegistry::new();
    let mut config = config(2, Some(100));
    config.algorithm = RateLimitAlgorithm::SlidingWindow;
    let subject = subject();
    let now = Instant::now();

    assert!(registry.check_at(&config, &subject, None, now).allowed);
    assert!(registry.check_at(&config, &subject, None, now).allowed);
    assert!(!registry.check_at(&config, &subject, None, now).allowed);
}

#[test]
fn counter_keys_separate_the_documented_scopes() {
    let subject = subject();
    let mut config = config(1, None);

    config.scope = crate::domain::model::RateLimitScope::Tenant;
    let tenant_key = RateLimiterRegistry::key_for(&config, &subject, None);
    config.scope = crate::domain::model::RateLimitScope::User;
    let user_key = RateLimiterRegistry::key_for(&config, &subject, None);
    config.scope = crate::domain::model::RateLimitScope::Ip;
    let ip_key = RateLimiterRegistry::key_for(&config, &subject, Some("203.0.113.9"));
    config.scope = crate::domain::model::RateLimitScope::Route;
    let route_key = RateLimiterRegistry::key_for(&config, &subject, None);
    config.scope = crate::domain::model::RateLimitScope::Global;
    let global_key = RateLimiterRegistry::key_for(&config, &subject, None);

    let keys = [&tenant_key, &user_key, &ip_key, &route_key, &global_key];
    for (index, key) in keys.iter().enumerate() {
        // Every key is prefixed by its upstream so deletion can sweep by prefix.
        assert!(key.starts_with(&format!("upstream:{}:", subject.upstream_id)));
        for other in keys.iter().skip(index + 1) {
            assert_ne!(key, other, "scopes must not share a counter");
        }
    }
    assert!(ip_key.ends_with("203.0.113.9"));
}

#[test]
fn separate_tenants_do_not_share_a_counter() {
    let registry = RateLimiterRegistry::new();
    let config = config(1, Some(1));
    let now = Instant::now();
    let first = subject();
    let second = RateLimitSubject {
        tenant_id: Uuid::from_u128(99),
        ..first
    };

    assert!(registry.check_at(&config, &first, None, now).allowed);
    assert!(!registry.check_at(&config, &first, None, now).allowed);
    assert!(registry.check_at(&config, &second, None, now).allowed);
}

#[test]
fn forgetting_an_upstream_drops_its_counters() {
    let registry = RateLimiterRegistry::new();
    let config = config(1, Some(1));
    let subject = subject();
    let now = Instant::now();

    assert!(registry.check_at(&config, &subject, None, now).allowed);
    assert!(!registry.check_at(&config, &subject, None, now).allowed);

    registry.forget_upstream(subject.upstream_id);
    assert!(registry.check_at(&config, &subject, None, now).allowed);
}

#[test]
fn a_verdict_reports_the_configured_limit_and_a_reset_horizon() {
    let registry = RateLimiterRegistry::new();
    let config = config(4, Some(4));
    let verdict = registry.check(&config, &subject(), None);
    assert_eq!(verdict.limit, 4);
    assert_eq!(verdict.remaining, 3);
    assert!(verdict.reset_after_secs >= 1);
}

#[tokio::test]
async fn the_queue_strategy_waits_for_capacity_within_its_budget() {
    let registry = RateLimiterRegistry::new();
    let config = config(20, Some(1));
    let subject = subject();

    assert!(registry.check(&config, &subject, None).allowed);
    // 20 tokens/s replenishes one in 50ms, comfortably inside the budget.
    let verdict = registry
        .acquire_queued(&config, &subject, None, Duration::from_millis(500))
        .await;
    assert!(verdict.allowed);
}

#[tokio::test]
async fn the_queue_strategy_gives_up_at_the_deadline() {
    let registry = RateLimiterRegistry::new();
    let config = config(1, Some(1));
    let subject = RateLimitSubject {
        upstream_id: Uuid::from_u128(77),
        ..subject()
    };

    assert!(registry.check(&config, &subject, None).allowed);
    let verdict = registry
        .acquire_queued(&config, &subject, None, Duration::from_millis(30))
        .await;
    assert!(!verdict.allowed);
}
