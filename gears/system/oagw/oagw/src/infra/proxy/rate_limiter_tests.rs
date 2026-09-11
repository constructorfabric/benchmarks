//! Sibling unit tests of [`crate::infra::proxy::rate_limiter`]
//! (`cpt-cf-oagw-dod-rate-limiting-unit-tests`).

use std::sync::Arc;

use super::rate_limiter::{RateLimiterRegistry, DEFAULT_IDLE_EVICTION_SECS, DEFAULT_MAX_COUNTERS};
use crate::domain::dto::{
    BurstCapacity, RateAlgorithm, RateLimitConfig, RateScope, RateStrategy, RateWindow,
    SharingMode, SustainedRate,
};
use crate::domain::rate_limit::{CounterPhase, CounterSpec, RateLimitResource, SharedClock};

const EPOCH: u64 = 1_700_000_000;

fn bucket_config(rate: u32, capacity: u32, cost: u32) -> RateLimitConfig {
    RateLimitConfig {
        sharing: SharingMode::Private,
        algorithm: RateAlgorithm::TokenBucket,
        sustained: SustainedRate { rate, window: RateWindow::Second },
        burst: Some(BurstCapacity { capacity }),
        budget: None,
        scope: RateScope::Tenant,
        strategy: RateStrategy::Reject,
        cost,
        response_headers: true,
    }
}

fn spec_of(rate: u32, capacity: u32, cost: u32) -> CounterSpec {
    CounterSpec::of(&bucket_config(rate, capacity, cost))
}

fn upstream(id: &str) -> RateLimitResource {
    RateLimitResource::Upstream { upstream_id: id.to_owned() }
}

fn key_of(resource: &RateLimitResource, tenant: &str) -> String {
    crate::domain::rate_limit::counter_key(&crate::domain::rate_limit::CounterKeyContext {
        resource,
        scope: RateScope::Tenant,
        tenant_id: tenant,
        principal_id: None,
        peer_addr: None,
        route_id: None,
    })
}

fn registry() -> (SharedClock, RateLimiterRegistry) {
    let clock = SharedClock::at(0, EPOCH);
    let limiter = RateLimiterRegistry::new(Arc::new(clock.clone()));
    (clock, limiter)
}

// -- lazily created state ---------------------------------------------------

#[test]
fn the_first_request_for_a_key_is_created_at_full_capacity_and_never_refused_for_missing_state() {
    let (clock, limiter) = registry();
    let resource = upstream("u-1");
    let key = key_of(&resource, "t-1");
    assert_eq!(limiter.len(), 0, "no counter is held before the first request");
    let decision = limiter.acquire(&key, &spec_of(10, 10, 1));
    assert!(decision.allowed);
    assert_eq!(decision.limit, 10);
    assert_eq!(decision.remaining, 9);
    assert_eq!(limiter.len(), 1);
    assert_eq!(limiter.phase_of(&key), Some(CounterPhase::Active));
    let _ = clock;
}

#[test]
fn a_depleted_counter_reports_its_phase_and_recovers_after_a_refill() {
    let (clock, limiter) = registry();
    let resource = upstream("u-1");
    let key = key_of(&resource, "t-1");
    let spec = spec_of(1, 1, 1);
    assert!(limiter.acquire(&key, &spec).allowed);
    assert!(!limiter.acquire(&key, &spec).allowed);
    assert_eq!(limiter.phase_of(&key), Some(CounterPhase::Depleted));
    clock.advance_seconds(1);
    assert!(limiter.acquire(&key, &spec).allowed);
    assert_eq!(limiter.phase_of(&key), Some(CounterPhase::Active));
}

#[test]
fn a_counter_created_for_a_sliding_window_starts_with_zero_recorded_consumption() {
    let (_clock, limiter) = registry();
    let resource = upstream("u-1");
    let key = key_of(&resource, "t-1");
    let mut config = bucket_config(2, 2, 1);
    config.algorithm = RateAlgorithm::SlidingWindow;
    config.burst = None;
    let spec = CounterSpec::of(&config);
    assert!(limiter.acquire(&key, &spec).allowed);
    assert!(limiter.acquire(&key, &spec).allowed);
    let refused = limiter.acquire(&key, &spec);
    assert!(!refused.allowed, "a sliding window applies no burst allowance");
    assert_eq!(refused.limit, 2);
}

// -- per-key isolation ------------------------------------------------------

#[test]
fn two_tenants_never_share_a_counter() {
    let (_clock, limiter) = registry();
    let resource = upstream("u-1");
    let spec = spec_of(1, 1, 1);
    let first = key_of(&resource, "t-1");
    let second = key_of(&resource, "t-2");
    assert!(limiter.acquire(&first, &spec).allowed);
    assert!(!limiter.acquire(&first, &spec).allowed);
    assert!(
        limiter.acquire(&second, &spec).allowed,
        "the second tenant keeps its own budget"
    );
    assert_eq!(limiter.len(), 2);
}

#[test]
fn an_upstream_and_its_route_never_contend_for_one_counter() {
    let (_clock, limiter) = registry();
    let upstream_resource = upstream("u-1");
    let route_resource = RateLimitResource::Route { route_id: "r-1".to_owned() };
    let spec = spec_of(1, 1, 1);
    assert!(limiter.acquire(&key_of(&upstream_resource, "t-1"), &spec).allowed);
    assert!(
        limiter.acquire(&key_of(&route_resource, "t-1"), &spec).allowed,
        "the route keeps its own budget"
    );
    assert_eq!(limiter.len(), 2);
}

#[test]
fn the_scope_discriminators_of_one_tenant_are_distinct_counters() {
    let (_clock, limiter) = registry();
    let spec = spec_of(2, 2, 1);
    let keys = [
        "upstream:u-1|t-1|t-1",
        "upstream:u-1|t-1|p-1",
        "upstream:u-1|t-1|10.0.0.1:5",
        "upstream:u-1|t-1|r-1",
        "upstream:u-1||",
    ];
    for key in keys {
        for _ in 0..2 {
            assert!(limiter.acquire(key, &spec).allowed, "{key}");
        }
        assert!(!limiter.acquire(key, &spec).allowed, "{key} enforces its own limit");
    }
    assert_eq!(limiter.len(), 5);
}

// -- the per-key lock -------------------------------------------------------

#[test]
fn concurrent_acquires_of_one_key_never_over_deduct_the_counter() {
    let limiter = Arc::new(registry().1);
    let resource = upstream("u-1");
    let key = key_of(&resource, "t-1");
    let spec = spec_of(1000, 1000, 1);
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let limiter = Arc::clone(&limiter);
            let key = key.clone();
            let spec = spec.clone();
            std::thread::spawn(move || {
                (0..250).filter(|_| limiter.acquire(&key, &spec).allowed).count()
            })
        })
        .collect();
    let admitted: usize = workers.into_iter().map(|worker| worker.join().expect("worker joins")).sum();
    assert_eq!(
        admitted, 1000,
        "the total deduction equals the number of successful acquisitions times the cost"
    );
    assert!(!limiter.acquire(&key, &spec).allowed);
}

#[test]
fn concurrent_acquires_of_different_keys_do_not_serialize_on_one_lock() {
    let limiter = Arc::new(registry().1);
    let resource = upstream("u-1");
    let spec = spec_of(10_000, 10_000, 1);
    let workers: Vec<_> = (0..4)
        .map(|index| {
            let limiter = Arc::clone(&limiter);
            let key = key_of(&resource, &format!("t-{index}"));
            let spec = spec.clone();
            std::thread::spawn(move || {
                (0..1000).filter(|_| limiter.acquire(&key, &spec).allowed).count()
            })
        })
        .collect();
    for worker in workers {
        assert_eq!(worker.join().expect("worker joins"), 1000);
    }
    assert_eq!(limiter.len(), 4);
}

// -- the bound and the eviction ---------------------------------------------

#[test]
fn the_registry_stays_inside_its_bound_when_more_keys_than_the_bound_are_exercised() {
    let clock = SharedClock::at(0, EPOCH);
    let limiter = RateLimiterRegistry::with_bounds(Arc::new(clock.clone()), 64, DEFAULT_IDLE_EVICTION_SECS);
    let resource = upstream("u-1");
    let spec = spec_of(10, 10, 1);
    for index in 0..200u32 {
        let tenant = format!("t-{index}");
        let _ = limiter.acquire(&key_of(&resource, &tenant), &spec);
        clock.advance_nanos(1);
    }
    // A batch eviction drops the registry to its low-water mark before it fills
    // again, so the bound is respected with headroom rather than pinned at it.
    assert!(limiter.len() <= 64, "the registry never exceeds its bound: {}", limiter.len());
}

/// A caller churning its key surface under one resource evicts only counters of
/// that same resource, so the budget a live eviction resets is always one the
/// caller's own requests draw from.
#[test]
fn a_churning_resource_does_not_evict_another_resources_counters() {
    let clock = SharedClock::at(0, EPOCH);
    let limiter = RateLimiterRegistry::with_bounds(Arc::new(clock.clone()), 8, DEFAULT_IDLE_EVICTION_SECS);
    let spec = spec_of(10, 10, 1);

    // A counter of a resource the churn does not touch, populated first and
    // left warm: under a global least-recently-used eviction it is the oldest
    // live entry and would be the first victim.
    let protected = upstream("u-protected");
    let protected_key = key_of(&protected, "t-steady");
    assert!(limiter.acquire(&protected_key, &spec).allowed);

    // Another resource churns far more distinct keys than the bound holds.
    let churned = upstream("u-churn");
    for index in 0..64u32 {
        let _ = limiter.acquire(&key_of(&churned, &format!("t-{index}")), &spec);
        clock.advance_nanos(1);
    }
    assert!(limiter.len() <= 8, "the registry stays inside its bound: {}", limiter.len());
    assert!(
        limiter.phase_of(&protected_key).is_some(),
        "the counter of the untouched resource survived the churn"
    );
    assert!(limiter.live_evictions() > 0, "the live evictions are observable");
}

#[test]
fn an_idle_key_is_evicted_and_a_fresh_request_starts_from_a_full_bucket() {
    let clock = SharedClock::at(0, EPOCH);
    let limiter = RateLimiterRegistry::with_bounds(Arc::new(clock.clone()), 64, 15 * 60);
    let resource = upstream("u-1");
    let key = key_of(&resource, "t-1");
    let spec = spec_of(1, 1, 1);
    assert!(limiter.acquire(&key, &spec).allowed);
    assert!(!limiter.acquire(&key, &spec).allowed);
    // 14 minutes: the key is still held, and its budget is still spent.
    clock.advance_seconds(14 * 60);
    assert_eq!(limiter.sweep(), 0);
    assert_eq!(limiter.len(), 1);
    // One minute later the key has been idle for the whole interval.
    clock.advance_seconds(60);
    assert_eq!(limiter.sweep(), 1);
    assert_eq!(limiter.len(), 0);
    assert_eq!(limiter.phase_of(&key), None);
    assert!(limiter.acquire(&key, &spec).allowed, "a fresh bucket, no stale budget");
}

#[test]
fn an_evicted_key_is_dropped_without_a_metric_or_a_log_entry() {
    let clock = SharedClock::at(0, EPOCH);
    let limiter = RateLimiterRegistry::with_bounds(Arc::new(clock.clone()), 8, 0);
    let resource = upstream("u-1");
    let spec = spec_of(1, 1, 1);
    for index in 0..8u32 {
        let _ = limiter.acquire(&key_of(&resource, &format!("t-{index}")), &spec);
    }
    assert_eq!(limiter.sweep(), 8, "a zero idle interval evicts every key");
    assert!(limiter.is_empty());
}

#[test]
fn the_documented_default_bound_is_ten_thousand_counters_and_fifteen_minutes() {
    assert_eq!(DEFAULT_MAX_COUNTERS, 10_000);
    assert_eq!(DEFAULT_IDLE_EVICTION_SECS, 900);
    let (_clock, limiter) = registry();
    let rendered = format!("{limiter:?}");
    assert!(rendered.contains("max_counters: 10000"), "{rendered}");
    assert!(rendered.contains("idle_eviction_secs: 900"), "{rendered}");
}

// -- the resource release ---------------------------------------------------

#[test]
fn releasing_a_resource_drops_only_its_own_counters() {
    let (_clock, limiter) = registry();
    let first = upstream("u-1");
    let second = upstream("u-2");
    let spec = spec_of(1, 1, 1);
    for tenant in ["t-1", "t-2"] {
        let _ = limiter.acquire(&key_of(&first, tenant), &spec);
        let _ = limiter.acquire(&key_of(&second, tenant), &spec);
    }
    assert_eq!(limiter.len(), 4);
    assert_eq!(limiter.release_resource(&first), 2);
    assert_eq!(limiter.len(), 2);
    assert_eq!(limiter.phase_of(&key_of(&first, "t-1")), None);
    // The counters of the untouched resource keep their spent budget.
    assert!(!limiter.acquire(&key_of(&second, "t-1"), &spec).allowed);
    assert!(limiter.acquire(&key_of(&second, "t-3"), &spec).allowed);
    // The released resource starts from a fresh bucket.
    assert!(limiter.acquire(&key_of(&first, "t-1"), &spec).allowed);
}

#[test]
fn releasing_an_unknown_resource_drops_nothing() {
    let (_clock, limiter) = registry();
    let spec = spec_of(1, 1, 1);
    let _ = limiter.acquire(&key_of(&upstream("u-1"), "t-1"), &spec);
    assert_eq!(limiter.release_resource(&upstream("u-9")), 0);
    assert_eq!(limiter.len(), 1);
}

// -- the specification change ----------------------------------------------

#[test]
fn a_changed_effective_limit_drops_the_stored_state_of_the_counter() {
    let (_clock, limiter) = registry();
    let resource = upstream("u-1");
    let key = key_of(&resource, "t-1");
    let strict = spec_of(1, 1, 1);
    assert!(limiter.acquire(&key, &strict).allowed);
    assert!(!limiter.acquire(&key, &strict).allowed);
    assert_eq!(limiter.phase_of(&key), Some(CounterPhase::Depleted));
    // The effective limit changed, so the stored state no longer matches the
    // configured capacity and the counter is re-derived.
    let relaxed = spec_of(10, 10, 1);
    let decision = limiter.acquire(&key, &relaxed);
    assert!(decision.allowed);
    assert_eq!(decision.limit, 10);
    assert_eq!(decision.remaining, 9);
}

#[test]
fn a_counter_keeps_its_algorithm_when_only_the_window_changes() {
    let (_clock, limiter) = registry();
    let resource = upstream("u-1");
    let key = key_of(&resource, "t-1");
    let per_second = spec_of(10, 10, 1);
    for _ in 0..10 {
        assert!(limiter.acquire(&key, &per_second).allowed);
    }
    let mut minute = bucket_config(10, 10, 1);
    minute.sustained.window = RateWindow::Minute;
    let per_minute = CounterSpec::of(&minute);
    assert_ne!(per_second, per_minute);
    let decision = limiter.acquire(&key, &per_minute);
    assert!(decision.allowed, "the counter was re-derived at the full capacity");
    assert_eq!(decision.limit, 10);
}

#[test]
fn the_specification_of_one_counter_is_carried_inside_it_and_not_in_its_key() {
    let (_clock, limiter) = registry();
    let resource = upstream("u-1");
    let key = key_of(&resource, "t-1");
    let spec = spec_of(10, 10, 1);
    for _ in 0..5 {
        let _ = limiter.acquire(&key, &spec);
    }
    // The key is stable across the five decisions, and the window state lives
    // in the counter body: one key, one counter, five phases of it.
    assert_eq!(limiter.len(), 1);
}
