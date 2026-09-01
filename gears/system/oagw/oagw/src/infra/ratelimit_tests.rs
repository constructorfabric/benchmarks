//! Token bucket and circuit breaker tests.

use crate::domain::model::{
    BurstCapacity, RateAlgorithm, RateLimitConfig, RateScope, RateStrategy, RateWindow,
    SharingMode, SustainedRate,
};
use crate::infra::ratelimit::{CircuitBreaker, CircuitState, RateLimiterRegistry};

fn limit(rate: u32, capacity: Option<u32>) -> RateLimitConfig {
    RateLimitConfig {
        sharing: SharingMode::Private,
        algorithm: RateAlgorithm::TokenBucket,
        sustained: SustainedRate {
            rate,
            window: RateWindow::Second,
        },
        burst: capacity.map(|capacity| BurstCapacity { capacity }),
        scope: RateScope::Tenant,
        strategy: RateStrategy::Reject,
        cost: 1,
        response_headers: true,
    }
}

#[test]
fn bucket_allows_up_to_capacity_then_rejects() {
    let registry = RateLimiterRegistry::new();
    let cfg = limit(1, Some(3));
    assert!(registry.check("k", &cfg).is_allowed());
    assert!(registry.check("k", &cfg).is_allowed());
    assert!(registry.check("k", &cfg).is_allowed());
    let rejected = registry.check("k", &cfg);
    assert!(!rejected.is_allowed());
    assert_eq!(rejected.limit(), 3);
    assert_eq!(rejected.retry_after(), 1);
}

#[test]
fn single_token_bucket_is_one_per_second() {
    let registry = RateLimiterRegistry::new();
    let cfg = limit(1, None);
    let first = registry.check("k", &cfg);
    assert_eq!(first.limit(), 1);
    assert_eq!(first.remaining(), 0);
    assert!(!registry.check("k", &cfg).is_allowed());
}

#[test]
fn buckets_are_isolated_by_key() {
    let registry = RateLimiterRegistry::new();
    let cfg = limit(1, Some(1));
    assert!(registry.check("tenant-a", &cfg).is_allowed());
    assert!(!registry.check("tenant-a", &cfg).is_allowed());
    assert!(registry.check("tenant-b", &cfg).is_allowed());
}

#[test]
fn scope_key_is_free_of_credential_material() {
    let upstream = uuid::Uuid::new_v4();
    let tenant = uuid::Uuid::new_v4();
    let subject = uuid::Uuid::new_v4();
    let route = uuid::Uuid::new_v4();
    assert_eq!(
        RateLimiterRegistry::scope_key(RateScope::Global, upstream, None, tenant, subject, None),
        format!("upstream:{upstream}:global:{tenant}")
    );
    assert_eq!(
        RateLimiterRegistry::scope_key(RateScope::Tenant, upstream, None, tenant, subject, None),
        format!("upstream:{upstream}:tenant:{tenant}")
    );
    assert_eq!(
        RateLimiterRegistry::scope_key(RateScope::User, upstream, None, tenant, subject, None),
        format!("upstream:{upstream}:user:{tenant}:{subject}")
    );
    assert_eq!(
        RateLimiterRegistry::scope_key(
            RateScope::Ip,
            upstream,
            None,
            tenant,
            subject,
            Some("10.0.0.1")
        ),
        format!("upstream:{upstream}:ip:{tenant}:10.0.0.1")
    );
    assert_eq!(
        RateLimiterRegistry::scope_key(RateScope::Ip, upstream, None, tenant, subject, None),
        format!("upstream:{upstream}:ip:{tenant}:unknown")
    );
    assert_eq!(
        RateLimiterRegistry::scope_key(
            RateScope::Route,
            upstream,
            Some(route),
            tenant,
            subject,
            None
        ),
        format!("route:{route}:route:{tenant}")
    );
}

#[test]
fn scope_keys_are_isolated_per_upstream_and_tenant() {
    let tenant = uuid::Uuid::new_v4();
    let other_tenant = uuid::Uuid::new_v4();
    let first = uuid::Uuid::new_v4();
    let second = uuid::Uuid::new_v4();
    for scope in [
        RateScope::Global,
        RateScope::Tenant,
        RateScope::Ip,
        RateScope::Route,
    ] {
        assert_ne!(
            RateLimiterRegistry::scope_key(scope, first, None, tenant, subject_of(), None),
            RateLimiterRegistry::scope_key(scope, second, None, tenant, subject_of(), None),
            "{scope:?} must be keyed per upstream"
        );
        assert_ne!(
            RateLimiterRegistry::scope_key(scope, first, None, tenant, subject_of(), None),
            RateLimiterRegistry::scope_key(scope, first, None, other_tenant, subject_of(), None),
            "{scope:?} must be keyed per tenant"
        );
    }
    // The `user` scope already carries the tenant in its scope value.
    assert_ne!(
        RateLimiterRegistry::scope_key(RateScope::User, first, None, tenant, subject_of(), None),
        RateLimiterRegistry::scope_key(RateScope::User, second, None, tenant, subject_of(), None)
    );
}

fn subject_of() -> uuid::Uuid {
    uuid::Uuid::new_v4()
}

#[test]
fn a_tightened_limit_takes_effect_immediately() {
    let registry = RateLimiterRegistry::new();
    let loose = limit(10, Some(10));
    let tight = limit(2, Some(2));
    for _ in 0..10 {
        assert!(registry.check("k", &loose).is_allowed());
    }
    // The bucket is empty after the loose limit; the tightened capacity must
    // replace the stored one instead of being widened by `max()`.
    assert!(!registry.check("k", &tight).is_allowed());
    assert_eq!(registry.check("k", &tight).limit(), 2);
}

#[test]
fn the_registry_is_bounded_and_evicts_the_oldest_bucket() {
    let registry = RateLimiterRegistry::new();
    let cfg = limit(1, Some(1));
    for index in 0..(crate::infra::ratelimit::MAX_BUCKETS + 64) {
        registry.check(&format!("bucket-{index}"), &cfg);
    }
    assert_eq!(
        registry.bucket_count(),
        crate::infra::ratelimit::MAX_BUCKETS,
        "the registry must not grow past its bound"
    );
    // The very first bucket was evicted, a recent one is still there.
    assert_eq!(registry.usage_ratio("bucket-0", 1), 0.0);
    assert!(
        registry.usage_ratio(
            &format!("bucket-{}", crate::infra::ratelimit::MAX_BUCKETS + 63),
            1
        ) > 0.0
    );
}

#[test]
fn usage_ratio_and_forget() {
    let registry = RateLimiterRegistry::new();
    let cfg = limit(1, Some(4));
    registry.check("u", &cfg);
    assert!((registry.usage_ratio("u", 4) - 0.25).abs() < 0.01);
    registry.forget("u");
    assert_eq!(registry.usage_ratio("u", 4), 0.0);
}

#[test]
fn breaker_opens_after_threshold_and_recovers() {
    let breaker = CircuitBreaker::new(2, std::time::Duration::from_millis(20));
    assert_eq!(breaker.state(), CircuitState::Closed);
    assert!(breaker.record_failure().is_none());
    let opened = breaker.record_failure();
    assert_eq!(opened, Some((CircuitState::Closed, CircuitState::Open)));
    assert_eq!(breaker.state(), CircuitState::Open);
    assert!(breaker.retry_after_seconds() <= 1);
    std::thread::sleep(std::time::Duration::from_millis(30));
    assert_eq!(breaker.state(), CircuitState::HalfOpen);
    assert!(breaker.record_success().is_some());
    assert_eq!(breaker.state(), CircuitState::Closed);
    assert_eq!(breaker.retry_after_seconds(), 0);
}

#[test]
fn breaker_reports_transition_once() {
    let breaker = CircuitBreaker::default_breaker();
    for _ in 0..4 {
        assert!(breaker.record_failure().is_none());
    }
    assert_eq!(
        breaker.record_failure(),
        Some((CircuitState::Closed, CircuitState::Open))
    );
    // Still open: a further failure does not re-report the transition.
    assert!(breaker.record_failure().is_none());
}

#[test]
fn only_one_half_open_probe_is_admitted() {
    let breaker = CircuitBreaker::new(1, std::time::Duration::from_millis(10));
    assert_eq!(
        breaker.record_failure(),
        Some((CircuitState::Closed, CircuitState::Open))
    );
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert_eq!(breaker.state(), CircuitState::HalfOpen);

    assert!(breaker.begin_probe(), "first probe admitted");
    assert!(!breaker.begin_probe(), "a second probe must be gated");
    assert!(!breaker.begin_probe());
    // Releasing the probe frees the slot again.
    breaker.end_probe();
    assert!(breaker.begin_probe());
}

#[test]
fn a_failing_probe_needs_the_threshold_to_reopen() {
    let breaker = CircuitBreaker::new(2, std::time::Duration::from_millis(10));
    // Two consecutive failures open the breaker.
    breaker.record_failure();
    assert_eq!(
        breaker.record_failure(),
        Some((CircuitState::Closed, CircuitState::Open))
    );
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert_eq!(breaker.state(), CircuitState::HalfOpen);
    {
        assert!(breaker.begin_probe());
        assert!(breaker.record_failure().is_none());
        breaker.end_probe();
    }
    // The probe failed once: the breaker is still half-open, and a second
    // consecutive failure re-opens it with a fresh cooldown.
    assert_eq!(breaker.state(), CircuitState::HalfOpen);
    {
        assert!(breaker.begin_probe());
        assert_eq!(
            breaker.record_failure(),
            Some((CircuitState::HalfOpen, CircuitState::Open))
        );
        breaker.end_probe();
    }
    assert_eq!(breaker.state(), CircuitState::Open);
}

#[test]
fn refill_is_based_on_a_monotonic_clock() {
    // Two checks in quick succession must consume one token each: the elapsed
    // time is derived from a monotonic source, so a wall-clock step cannot
    // grant extra tokens.
    let registry = RateLimiterRegistry::new();
    let cfg = limit(1, Some(1));
    assert!(registry.check("mono", &cfg).is_allowed());
    let second = registry.check("mono", &cfg);
    assert!(!second.is_allowed());
    assert_eq!(second.retry_after(), 1);
    assert!(crate::infra::ratelimit::elapsed_millis(0) < 60_000);
}
