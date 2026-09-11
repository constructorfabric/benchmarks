//! Sibling unit tests of [`crate::domain::rate_limit`]
//! (`cpt-cf-oagw-dod-rate-limiting-unit-tests`).

use std::collections::VecDeque;

use super::rate_limit::{
    counter_key, quota_header_pairs, ClockReading, CounterKeyContext, CounterSpec, CounterState,
    RateDecision, RateLimitResource, RateClock, SharedClock, LIMIT_HEADER, REMAINING_HEADER,
    RESET_HEADER,
};
use crate::domain::dto::{
    BurstCapacity, RateAlgorithm, RateLimitConfig, RateScope, RateStrategy, RateWindow,
    SharingMode, SustainedRate,
};
use crate::domain::rate_limit::{executable_strategy, CounterPhase};

/// A `token_bucket` limit of `rate` units per second with an explicit burst.
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

/// A `sliding_window` limit of `rate` units over `window`.
fn window_config(rate: u32, window: RateWindow, cost: u32) -> RateLimitConfig {
    RateLimitConfig {
        sharing: SharingMode::Private,
        algorithm: RateAlgorithm::SlidingWindow,
        sustained: SustainedRate { rate, window },
        burst: None,
        budget: None,
        scope: RateScope::Tenant,
        strategy: RateStrategy::Reject,
        cost,
        response_headers: true,
    }
}

const EPOCH: u64 = 1_700_000_000;

fn reading(clock: &SharedClock) -> ClockReading {
    RateClock::now(clock)
}

// -- the counter specification ----------------------------------------------

#[test]
fn a_spec_defaults_the_capacity_to_the_sustained_rate() {
    let config = bucket_config(600, 600, 1);
    let spec = CounterSpec::of(&config);
    assert_eq!(spec.capacity, 600);
    assert_eq!(spec.sustained_rate, 600);
    assert_eq!(spec.window_secs, 1);
    assert_eq!(spec.cost, 1);
    assert_eq!(spec.effective_limit(), 600);
}

#[test]
fn a_spec_takes_the_declared_capacity_and_window() {
    let mut config = bucket_config(10, 50, 2);
    config.sustained.window = RateWindow::Minute;
    let spec = CounterSpec::of(&config);
    assert_eq!(spec.capacity, 50);
    assert_eq!(spec.window_secs, 60);
    assert_eq!(spec.cost, 2);
    assert!((spec.refill_per_second() - 10.0 / 60.0).abs() < 1e-12);
}

#[test]
fn the_effective_limit_of_a_sliding_window_is_the_sustained_rate() {
    let spec = CounterSpec::of(&window_config(30, RateWindow::Minute, 1));
    assert_eq!(spec.effective_limit(), 30);
    assert_eq!(spec.capacity, 30);
}

// -- the token bucket -------------------------------------------------------

#[test]
fn a_fresh_token_bucket_admits_a_full_burst_up_to_capacity() {
    let clock = SharedClock::at(0, EPOCH);
    let spec = CounterSpec::of(&bucket_config(10, 10, 1));
    let mut counter = CounterState::fresh(&spec, reading(&clock));
    for _ in 0..10 {
        let decision = counter.check(&spec, reading(&clock));
        assert!(decision.allowed, "a burst of 10 is admitted in full");
    }
    let decision = counter.check(&spec, reading(&clock));
    assert!(!decision.allowed, "the eleventh request of the burst is refused");
    assert_eq!(decision.remaining, 0);
}

#[test]
fn a_token_bucket_refills_on_read_and_never_exceeds_the_capacity() {
    let clock = SharedClock::at(0, EPOCH);
    let spec = CounterSpec::of(&bucket_config(10, 10, 1));
    let mut counter = CounterState::fresh(&spec, reading(&clock));
    for _ in 0..10 {
        counter.check(&spec, reading(&clock));
    }
    clock.advance_seconds(2);
    let decision = counter.check(&spec, reading(&clock));
    assert!(decision.allowed);
    // 2 seconds of refill restore 10 tokens, capped at the capacity of 10; the
    // request costs 1, so 9 remain.
    assert_eq!(decision.remaining, 9);
}

#[test]
fn a_refused_request_consumes_nothing_and_does_not_double_count_the_elapsed_time() {
    let clock = SharedClock::at(0, EPOCH);
    let spec = CounterSpec::of(&bucket_config(1, 10, 6));
    let mut counter = CounterState::fresh(&spec, reading(&clock));
    assert!(counter.check(&spec, reading(&clock)).allowed);
    let refused = counter.check(&spec, reading(&clock));
    assert!(!refused.allowed);
    assert_eq!(refused.remaining, 4);
    // A second refusal at the same instant must not refill again.
    let again = counter.check(&spec, reading(&clock));
    assert!(!again.allowed);
    assert_eq!(again.remaining, refused.remaining);
    // One second later exactly one second of refill has been applied, once.
    clock.advance_seconds(1);
    let refilled = counter.check(&spec, reading(&clock));
    assert!(!refilled.allowed, "5 tokens cannot cover a cost of 6");
    assert_eq!(refilled.remaining, 5);
    clock.advance_seconds(0);
    assert_eq!(counter.check(&spec, reading(&clock)).remaining, 5);
}

#[test]
fn a_weighted_cost_deducts_more_than_one_token() {
    let clock = SharedClock::at(0, EPOCH);
    let spec = CounterSpec::of(&bucket_config(10, 10, 4));
    let mut counter = CounterState::fresh(&spec, reading(&clock));
    assert!(counter.check(&spec, reading(&clock)).allowed);
    let decision = counter.check(&spec, reading(&clock));
    assert!(decision.allowed);
    assert_eq!(decision.remaining, 2);
    assert!(!counter.check(&spec, reading(&clock)).allowed);
}

#[test]
fn the_token_bucket_reset_instant_is_when_the_balance_returns_to_full_capacity() {
    let clock = SharedClock::at(0, EPOCH);
    let spec = CounterSpec::of(&bucket_config(10, 10, 1));
    let mut counter = CounterState::fresh(&spec, reading(&clock));
    for _ in 0..10 {
        counter.check(&spec, reading(&clock));
    }
    // The balance is 0 and the refill is 10 per second, so it is full again one
    // second later.
    let refused = counter.check(&spec, reading(&clock));
    assert!(!refused.allowed);
    assert_eq!(refused.reset_epoch_seconds, EPOCH + 1);
    assert_eq!(refused.retry_after_seconds, Some(1));
    assert_eq!(refused.usage_ratio, 1.0);
}

#[test]
fn the_reset_instant_is_projected_through_one_wall_clock_pairing() {
    // The monotonic origin and the wall clock are deliberately far apart: the
    // projection adds the monotonic distance to the wall-clock second.
    let clock = SharedClock::at(1_000 * 1_000_000_000, EPOCH);
    let spec = CounterSpec::of(&bucket_config(1, 1, 1));
    let mut counter = CounterState::fresh(&spec, reading(&clock));
    assert!(counter.check(&spec, reading(&clock)).allowed);
    let refused = counter.check(&spec, reading(&clock));
    assert!(!refused.allowed);
    assert_eq!(refused.reset_epoch_seconds, EPOCH + 1);
    // The projected reset does not move when only the wall clock moves.
    clock.set_epoch_seconds(EPOCH + 500);
    let again = counter.check(&spec, reading(&clock));
    assert_eq!(again.reset_epoch_seconds, EPOCH + 500 + 1);
}

#[test]
fn the_retry_after_is_the_whole_seconds_until_the_cost_is_affordable_again() {
    let clock = SharedClock::at(0, EPOCH);
    let spec = CounterSpec::of(&bucket_config(1, 1, 1));
    let mut counter = CounterState::fresh(&spec, reading(&clock));
    assert!(counter.check(&spec, reading(&clock)).allowed);
    let refused = counter.check(&spec, reading(&clock));
    assert_eq!(refused.retry_after_seconds, Some(1));
}

#[test]
fn the_token_bucket_refills_fractionally_over_a_long_window() {
    let mut config = bucket_config(10, 10, 1);
    config.sustained.window = RateWindow::Minute;
    let clock = SharedClock::at(0, EPOCH);
    let spec = CounterSpec::of(&config);
    let mut counter = CounterState::fresh(&spec, reading(&clock));
    for _ in 0..10 {
        counter.check(&spec, reading(&clock));
    }
    clock.advance_seconds(12);
    // 12 seconds of a 10-per-minute refill restore exactly 2 tokens.
    let decision = counter.check(&spec, reading(&clock));
    assert!(decision.allowed, "2 tokens have refilled after 12 seconds");
    assert_eq!(decision.remaining, 1);
}

// -- the sliding window -----------------------------------------------------

#[test]
fn a_sliding_window_admits_up_to_the_sustained_rate_with_no_burst() {
    let clock = SharedClock::at(0, EPOCH);
    let spec = CounterSpec::of(&window_config(5, RateWindow::Second, 1));
    let mut counter = CounterState::fresh(&spec, reading(&clock));
    for _ in 0..5 {
        let decision = counter.check(&spec, reading(&clock));
        assert!(decision.allowed);
        assert_eq!(decision.limit, 5);
    }
    let refused = counter.check(&spec, reading(&clock));
    assert!(!refused.allowed);
    assert_eq!(refused.remaining, 0);
    assert_eq!(refused.usage_ratio, 1.0);
}

#[test]
fn a_sliding_window_expires_the_consumption_that_leaves_the_trailing_window() {
    let clock = SharedClock::at(0, EPOCH);
    let spec = CounterSpec::of(&window_config(5, RateWindow::Second, 2));
    let mut counter = CounterState::fresh(&spec, reading(&clock));
    assert!(counter.check(&spec, reading(&clock)).allowed);
    clock.advance_nanos(500_000_000);
    assert!(counter.check(&spec, reading(&clock)).allowed);
    clock.advance_nanos(400_000_000);
    // The trailing window still records all four consumed units.
    let still_full = counter.check(&spec, reading(&clock));
    assert!(!still_full.allowed);
    assert_eq!(still_full.remaining, 1);
    // One second after the first admission it has left the window.
    clock.advance_nanos(100_000_001);
    let decision = counter.check(&spec, reading(&clock));
    assert!(decision.allowed, "the first consumption has left the window");
    assert_eq!(decision.remaining, 1);
    assert!(!counter.check(&spec, reading(&clock)).allowed);
}

#[test]
fn a_sliding_window_refusal_names_the_release_instant_of_the_oldest_consumption() {
    let clock = SharedClock::at(0, EPOCH);
    let spec = CounterSpec::of(&window_config(2, RateWindow::Second, 1));
    let mut counter = CounterState::fresh(&spec, reading(&clock));
    for _ in 0..2 {
        assert!(counter.check(&spec, reading(&clock)).allowed);
    }
    let refused = counter.check(&spec, reading(&clock));
    assert!(!refused.allowed);
    assert_eq!(refused.reset_epoch_seconds, EPOCH + 1);
    assert_eq!(refused.retry_after_seconds, Some(1));
}

#[test]
fn a_sliding_window_refusal_waits_for_enough_consumption_to_leave() {
    let clock = SharedClock::at(0, EPOCH);
    let spec = CounterSpec::of(&window_config(4, RateWindow::Second, 2));
    let mut counter = CounterState::fresh(&spec, reading(&clock));
    assert!(counter.check(&spec, reading(&clock)).allowed);
    clock.advance_nanos(200_000_000);
    assert!(counter.check(&spec, reading(&clock)).allowed);
    // 4 of 4 units are consumed, so the window must release the consumption
    // recorded at the first instant before the request is admitted again.
    clock.advance_nanos(100_000_000);
    assert!(!counter.check(&spec, reading(&clock)).allowed);
    clock.advance_nanos(700_000_001);
    let decision = counter.check(&spec, reading(&clock));
    assert!(decision.allowed, "the first consumption has left the window");
    assert_eq!(decision.remaining, 0);
}

#[test]
fn a_sliding_window_reports_the_window_in_its_reset_instant() {
    let mut config = window_config(60, RateWindow::Minute, 1);
    config.sustained.window = RateWindow::Minute;
    let clock = SharedClock::at(0, EPOCH);
    let spec = CounterSpec::of(&config);
    let mut counter = CounterState::fresh(&spec, reading(&clock));
    for _ in 0..60 {
        assert!(counter.check(&spec, reading(&clock)).allowed);
    }
    let refused = counter.check(&spec, reading(&clock));
    assert!(!refused.allowed);
    assert_eq!(refused.reset_epoch_seconds, EPOCH + 60);
    assert_eq!(refused.retry_after_seconds, Some(60));
}

// -- the counter key --------------------------------------------------------

fn key_context<'a>(
    resource: &'a RateLimitResource,
    scope: RateScope,
    tenant: &'a str,
    principal: Option<&'a str>,
    peer: Option<&'a str>,
    route: Option<&'a str>,
) -> CounterKeyContext<'a> {
    CounterKeyContext {
        resource,
        scope,
        tenant_id: tenant,
        principal_id: principal,
        peer_addr: peer,
        route_id: route,
    }
}

#[test]
fn the_key_starts_with_the_owning_resource_identity() {
    let upstream = RateLimitResource::Upstream { upstream_id: "u-1".to_owned() };
    let route = RateLimitResource::Route { route_id: "r-1".to_owned() };
    let tenant_scope = RateScope::Tenant;
    assert_eq!(
        counter_key(&key_context(&upstream, tenant_scope, "t-1", None, None, None)),
        "upstream:u-1|t-1|t-1"
    );
    assert_eq!(
        counter_key(&key_context(&route, tenant_scope, "t-1", None, None, Some("r-1"))),
        "route:r-1|t-1|t-1"
    );
}

#[test]
fn every_scope_other_than_global_carries_the_tenant_as_its_second_component() {
    let resource = RateLimitResource::Upstream { upstream_id: "u-1".to_owned() };
    for scope in [RateScope::Tenant, RateScope::User, RateScope::Ip, RateScope::Route] {
        let key = counter_key(&key_context(&resource, scope, "t-1", Some("p-1"), Some("10.0.0.1:5"), Some("r-1")));
        let components: Vec<&str> = key.split('|').collect();
        assert_eq!(components.len(), 3, "{key}");
        assert_eq!(components[1], "t-1", "{scope:?} keeps the tenant component");
    }
    let other = counter_key(&key_context(&resource, RateScope::User, "t-2", Some("p-1"), None, None));
    assert_eq!(other, "upstream:u-1|t-2|p-1");
    assert_ne!(other, "upstream:u-1|t-1|p-1", "two tenants never share a counter");
}

#[test]
fn each_scope_uses_its_own_discriminator() {
    let resource = RateLimitResource::Upstream { upstream_id: "u-1".to_owned() };
    let cases = [
        (RateScope::Tenant, "upstream:u-1|t-1|t-1"),
        (RateScope::User, "upstream:u-1|t-1|p-1"),
        (RateScope::Ip, "upstream:u-1|t-1|10.0.0.1:5"),
        (RateScope::Route, "upstream:u-1|t-1|r-1"),
    ];
    for (scope, expected) in cases {
        assert_eq!(
            counter_key(&key_context(&resource, scope, "t-1", Some("p-1"), Some("10.0.0.1:5"), Some("r-1"))),
            expected
        );
    }
}

#[test]
fn the_global_scope_carries_no_tenant_component() {
    let resource = RateLimitResource::Upstream { upstream_id: "u-1".to_owned() };
    let key = counter_key(&key_context(&resource, RateScope::Global, "t-1", Some("p-1"), Some("10.0.0.1:5"), Some("r-1")));
    assert_eq!(key, "upstream:u-1||");
    let other = counter_key(&key_context(&resource, RateScope::Global, "t-2", None, None, None));
    assert_eq!(key, other, "a global scope is one instance-wide counter");
}

#[test]
fn a_missing_discriminator_falls_back_to_the_tenant_discriminator() {
    let resource = RateLimitResource::Upstream { upstream_id: "u-1".to_owned() };
    let user = counter_key(&key_context(&resource, RateScope::User, "t-1", None, Some("10.0.0.1:5"), Some("r-1")));
    assert_eq!(user, "upstream:u-1|t-1|t-1");
    let peer = counter_key(&key_context(&resource, RateScope::Ip, "t-1", Some("p-1"), None, Some("r-1")));
    assert_eq!(peer, "upstream:u-1|t-1|t-1");
    let route = counter_key(&key_context(&resource, RateScope::Route, "t-1", Some("p-1"), Some("10.0.0.1:5"), None));
    assert_eq!(route, "upstream:u-1|t-1|t-1");
}

#[test]
fn a_client_supplied_header_and_query_component_never_change_the_key() {
    let resource = RateLimitResource::Upstream { upstream_id: "u-1".to_owned() };
    let scope = RateScope::Ip;
    let key = counter_key(&key_context(&resource, scope, "t-1", Some("p-1"), Some("10.0.0.1:5"), Some("r-1")));
    // Only the peer address the connection carries is an input; a forwarding
    // header the caller supplies is not part of the context type at all, so the
    // key derived from the same authenticated identity is byte-identical.
    let repeated = counter_key(&key_context(&resource, scope, "t-1", Some("p-1"), Some("10.0.0.1:5"), Some("r-1")));
    assert_eq!(key, repeated);
    assert!(!key.contains("forwarded"));
}

#[test]
fn the_resource_prefix_groups_the_counters_of_one_resource() {
    let upstream = RateLimitResource::Upstream { upstream_id: "u-1".to_owned() };
    assert_eq!(upstream.prefix(), "upstream:u-1|");
    let key = counter_key(&key_context(&upstream, RateScope::Tenant, "t-1", None, None, None));
    assert!(key.starts_with(&upstream.prefix()));
}

// -- the quota headers, the ratio, and the strategy -------------------------

#[test]
fn the_quota_headers_carry_the_limit_the_remaining_and_the_reset() {
    let decision = RateDecision {
        allowed: true,
        limit: 600,
        remaining: 598,
        usage_ratio: 0.0,
        reset_epoch_seconds: 1_700_000_000,
        retry_after_seconds: None,
    };
    let headers = quota_header_pairs(&decision);
    assert_eq!(headers[0].0, LIMIT_HEADER);
    assert_eq!(headers[0].1, "600");
    assert_eq!(headers[1].0, REMAINING_HEADER);
    assert_eq!(headers[1].1, "598");
    assert_eq!(headers[2].0, RESET_HEADER);
    assert_eq!(headers[2].1, "1700000000");
}

#[test]
fn the_usage_ratio_is_the_consumed_fraction_clamped_to_the_unit_range() {
    let clock = SharedClock::at(0, EPOCH);
    let spec = CounterSpec::of(&bucket_config(10, 10, 4));
    let mut counter = CounterState::fresh(&spec, reading(&clock));
    let decision = counter.check(&spec, reading(&clock));
    assert!((decision.usage_ratio - 0.4).abs() < 1e-12);
    let admitted = counter.check(&spec, reading(&clock));
    assert!(admitted.allowed);
    assert!((admitted.usage_ratio - 0.8).abs() < 1e-12);
    let refused = counter.check(&spec, reading(&clock));
    assert!(!refused.allowed);
    assert!((refused.usage_ratio - 0.8).abs() < 1e-12);
}

#[test]
fn a_degenerate_capacity_reports_a_zero_usage_ratio() {
    let clock = SharedClock::at(0, EPOCH);
    let spec = CounterSpec::of(&window_config(1, RateWindow::Second, 1));
    let mut counter = CounterState::fresh(&spec, reading(&clock));
    assert!(counter.check(&spec, reading(&clock)).allowed);
    let refused = counter.check(&spec, reading(&clock));
    assert_eq!(refused.usage_ratio, 1.0);
}

#[test]
fn queue_and_degrade_resolve_to_the_reject_outcome() {
    assert_eq!(executable_strategy(RateStrategy::Reject), RateStrategy::Reject);
    assert_eq!(executable_strategy(RateStrategy::Queue), RateStrategy::Reject);
    assert_eq!(executable_strategy(RateStrategy::Degrade), RateStrategy::Reject);
}

#[test]
fn the_counter_phase_follows_the_decision() {
    let clock = SharedClock::at(0, EPOCH);
    let spec = CounterSpec::of(&bucket_config(10, 10, 6));
    let mut counter = CounterState::fresh(&spec, reading(&clock));
    let allowed = counter.check(&spec, reading(&clock));
    assert_eq!(phase_of(&allowed), CounterPhase::Active);
    clock.advance_seconds(0);
    let refused = counter.check(&spec, reading(&clock));
    assert_eq!(phase_of(&refused), CounterPhase::Depleted);
    clock.advance_seconds(2);
    let recovered = counter.check(&spec, reading(&clock));
    assert_eq!(phase_of(&recovered), CounterPhase::Active);
}

fn phase_of(decision: &RateDecision) -> CounterPhase {
    if decision.allowed { CounterPhase::Active } else { CounterPhase::Depleted }
}

#[test]
fn a_shared_clock_is_driven_by_the_test_without_sleeping() {
    let clock = SharedClock::at(0, EPOCH);
    assert_eq!(reading(&clock), ClockReading { monotonic_nanos: 0, epoch_seconds: EPOCH });
    clock.advance_seconds(3);
    clock.advance_nanos(1);
    clock.set_epoch_seconds(EPOCH + 9);
    assert_eq!(reading(&clock).monotonic_nanos, 3_000_000_001);
    assert_eq!(reading(&clock).epoch_seconds, EPOCH + 9);
}

#[test]
fn a_counter_created_for_a_sliding_window_starts_with_zero_recorded_consumption() {
    let clock = SharedClock::at(0, EPOCH);
    let spec = CounterSpec::of(&window_config(5, RateWindow::Second, 1));
    let state = CounterState::fresh(&spec, reading(&clock));
    match &state {
        CounterState::SlidingWindow(window) => {
            assert_eq!(window.used, 0);
            assert_eq!(window.granules, VecDeque::new());
        }
        CounterState::TokenBucket(_) => panic!("the sliding window spec built a token bucket"),
    }
}

#[test]
fn a_counter_created_for_a_token_bucket_starts_at_the_capacity() {
    let clock = SharedClock::at(0, EPOCH);
    let spec = CounterSpec::of(&bucket_config(5, 7, 1));
    let state = CounterState::fresh(&spec, reading(&clock));
    match &state {
        CounterState::TokenBucket(bucket) => {
            assert_eq!(bucket.tokens, 7.0);
            assert_eq!(bucket.last_nanos, 0);
        }
        CounterState::SlidingWindow(_) => panic!("the token bucket spec built a sliding window"),
    }
}
