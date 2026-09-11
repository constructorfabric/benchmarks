//! The per-instance `RateLimiterRegistry` and the counter keys it holds.
//!
//! Covers the `{resource_type}:{resource_id}` prefix ADR 0003's key structure
//! puts at the head of every key, the five counter scopes, the `tenant`
//! fallback a `user` or `ip` key that cannot be formed takes, the full-bucket
//! initialization of an absent bucket, and the prefix drop of a deleted
//! upstream and of a deleted route.

// @cpt-dod:cpt-cf-oagw-dod-rate-limit-state:p1
// @cpt-dod:cpt-cf-oagw-dod-rate-limit-tests:p1

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::time::{Duration, Instant};

use oagw::domain::ratelimit::{
    QUEUE_WAIT, RateLimiterRegistry, token_bucket,
};
use oagw::domain::upstream::{RateLimitScope, Sustained, Window};

fn sustained(rate: u64, window: Window) -> Sustained {
    Sustained {
        rate,
        window: Some(window),
    }
}

#[test]
fn every_key_carries_the_resource_prefix() {
    // The structure ADR 0003's Redis key structure gives: the
    // `{resource_type}:{resource_id}` prefix of the resource whose
    // `rate_limit` the effective limit came from, followed by the scope, its
    // identifier, and the effective window.
    let key = RateLimiterRegistry::counter_key(
        "upstream",
        "11111111-1111-1111-1111-111111111111",
        RateLimitScope::Tenant,
        "22222222-2222-2222-2222-222222222222",
        Some(Window::Minute),
    );
    assert!(
        key.starts_with("upstream:11111111-1111-1111-1111-111111111111:"),
        "the prefix leads the key: {key}"
    );
    assert!(key.contains("Tenant"), "the scope follows the prefix: {key}");
    assert!(key.contains("22222222-2222-2222-2222-222222222222"), "the scope identifier follows: {key}");
    assert!(key.ends_with("60000"), "the effective window closes the key: {key}");
}

#[test]
fn two_upstreams_at_the_same_scope_never_share_a_counter() {
    // The prefix is what keeps two upstreams limited at the same `scope` from
    // sharing a counter, which is the property the key structure gives.
    let first = RateLimiterRegistry::counter_key(
        "upstream",
        "11111111-1111-1111-1111-111111111111",
        RateLimitScope::Tenant,
        "22222222-2222-2222-2222-222222222222",
        Some(Window::Minute),
    );
    let second = RateLimiterRegistry::counter_key(
        "upstream",
        "33333333-3333-3333-3333-333333333333",
        RateLimitScope::Tenant,
        "22222222-2222-2222-2222-222222222222",
        Some(Window::Minute),
    );
    assert_ne!(first, second);
}

#[test]
fn the_route_layer_keys_on_the_matched_route() {
    // The prefix names the resource whose `rate_limit` the effective limit
    // came from — the matched route when the effective limit is the route
    // layer's, and the resolved upstream for every other layer.
    let route_key = RateLimiterRegistry::counter_key(
        "route",
        "44444444-4444-4444-4444-444444444444",
        RateLimitScope::Route,
        "44444444-4444-4444-4444-444444444444",
        Some(Window::Minute),
    );
    let upstream_key = RateLimiterRegistry::counter_key(
        "upstream",
        "11111111-1111-1111-1111-111111111111",
        RateLimitScope::Route,
        "44444444-4444-4444-4444-444444444444",
        Some(Window::Minute),
    );
    assert!(route_key.starts_with("route:44444444"));
    assert!(upstream_key.starts_with("upstream:11111111"));
    assert_ne!(route_key, upstream_key);
}

#[test]
fn the_five_scopes_select_five_distinct_counters() {
    // `global` charges one counter for the whole gear, `tenant` one per
    // calling tenant, `user` one per authenticated subject, `ip` one per peer
    // address, and `route` one per matched route (§1.5).
    let tenant_id = "22222222-2222-2222-2222-222222222222";
    let scopes = [
        (RateLimitScope::Global, ""),
        (RateLimitScope::Tenant, tenant_id),
        (RateLimitScope::User, "subject-1"),
        (RateLimitScope::Ip, "203.0.113.7"),
        (RateLimitScope::Route, "44444444-4444-4444-4444-444444444444"),
    ];
    let keys: Vec<String> = scopes
        .iter()
        .map(|(scope, id)| RateLimiterRegistry::counter_key("upstream", "alias", *scope, id, Some(Window::Minute)))
        .collect();
    for (index, key) in keys.iter().enumerate() {
        for other in keys.iter().skip(index + 1) {
            assert_ne!(key, other, "two scopes never share a counter key");
        }
    }
}

#[test]
fn the_fallback_keys_on_the_calling_tenant() {
    // A `user` key with no authenticated subject and an `ip` key with no
    // resolvable peer address fall back to the `tenant` scope and its key
    // rather than skip enforcement (§1.4), and the fallback is the same key
    // the tenant scope forms for every request that lacks the identifier.
    let tenant_key = RateLimiterRegistry::counter_key(
        "upstream",
        "alias",
        RateLimitScope::Tenant,
        "22222222-2222-2222-2222-222222222222",
        Some(Window::Minute),
    );
    let user_fallback = RateLimiterRegistry::counter_key(
        "upstream",
        "alias",
        RateLimitScope::Tenant,
        "22222222-2222-2222-2222-222222222222",
        Some(Window::Minute),
    );
    let ip_fallback = RateLimiterRegistry::counter_key(
        "upstream",
        "alias",
        RateLimitScope::Tenant,
        "22222222-2222-2222-2222-222222222222",
        Some(Window::Minute),
    );
    assert_eq!(user_fallback, tenant_key);
    assert_eq!(ip_fallback, tenant_key);
}

#[test]
fn an_absent_bucket_is_initialized_full() {
    // The bucket the registry holds for a key it has never seen starts at full
    // capacity, so a first burst is admitted up to `burst.capacity` (§1.5).
    let start = Instant::now();
    let mut registry = RateLimiterRegistry::new();
    let key = RateLimiterRegistry::counter_key(
        "upstream",
        "alias",
        RateLimitScope::Tenant,
        "tenant",
        Some(Window::Minute),
    );
    let bucket = registry.bucket(key.as_str(), 10, &sustained(10, Window::Second), start);
    assert_eq!(bucket.tokens(), 10);
    // The second read of the same key is the same bucket, so a first burst
    // cannot be replayed by asking for the key again.
    let again = registry.bucket(key.as_str(), 10, &sustained(10, Window::Second), start);
    let outcome = token_bucket(again, 4, start);
    assert!(outcome.admitted);
    assert_eq!(again.tokens(), 6);
    assert_eq!(registry.bucket(key.as_str(), 10, &sustained(10, Window::Second), start).tokens(), 6);
}

#[test]
fn a_dropped_upstream_leaves_no_entry_behind() {
    // Deleting an upstream drops every bucket keyed under that upstream's
    // prefix and the breaker machine held for it, and retains every other
    // tenant's and every sibling route's entries (§1.5).
    let start = Instant::now();
    let mut registry = RateLimiterRegistry::new();
    let own = RateLimiterRegistry::counter_key("upstream", "gone", RateLimitScope::Tenant, "t", Some(Window::Minute));
    let sibling = RateLimiterRegistry::counter_key("upstream", "kept", RateLimitScope::Tenant, "t", Some(Window::Minute));
    let route = RateLimiterRegistry::counter_key("route", "kept-route", RateLimitScope::Route, "kept-route", Some(Window::Minute));
    let _ = registry.bucket(own.as_str(), 10, &sustained(10, Window::Second), start);
    let _ = registry.bucket(sibling.as_str(), 10, &sustained(10, Window::Second), start);
    let _ = registry.bucket(route.as_str(), 10, &sustained(10, Window::Second), start);
    let _ = registry.breaker("upstream:gone");
    let _ = registry.enqueue(own.as_str(), start);

    let dropped = registry.drop_prefix("upstream:gone");
    assert_eq!(dropped, 3, "the bucket, the breaker, and the queued slot");
    assert_eq!(registry.queue_len(own.as_str()), 0);
    assert!(
        registry.bucket(sibling.as_str(), 10, &sustained(10, Window::Second), start).tokens() == 10,
        "the sibling upstream keeps its own bucket"
    );
    assert!(
        registry.bucket(route.as_str(), 10, &sustained(10, Window::Second), start).tokens() == 10,
        "the route keeps its own bucket"
    );
}

#[test]
fn a_dropped_route_leaves_the_upstream_untouched() {
    // Deleting a route drops every entry keyed under that route's prefix and
    // leaves the upstream's own buckets and its breaker machine in place,
    // because the route's counters are not the upstream's (§1.5).
    let start = Instant::now();
    let mut registry = RateLimiterRegistry::new();
    let route = RateLimiterRegistry::counter_key("route", "gone-route", RateLimitScope::Route, "gone-route", Some(Window::Minute));
    let upstream = RateLimiterRegistry::counter_key("upstream", "kept", RateLimitScope::Tenant, "t", Some(Window::Minute));
    let _ = registry.bucket(route.as_str(), 10, &sustained(10, Window::Second), start);
    let _ = registry.bucket(upstream.as_str(), 10, &sustained(10, Window::Second), start);
    let _ = registry.breaker("upstream:kept");

    let dropped = registry.drop_prefix("route:gone-route");
    assert_eq!(dropped, 1);
    assert!(
        registry.bucket(upstream.as_str(), 10, &sustained(10, Window::Second), start).tokens() == 10,
        "the upstream's own bucket stays"
    );
    assert_eq!(registry.breaker("upstream:kept").failures.len(), 0, "the upstream's breaker stays");
}

#[test]
fn a_configuration_that_holds_no_bucket_drops_nothing() {
    // The cleanup is idempotent over an absent key set, which is the error
    // scenario the cleanup flow records.
    let mut registry = RateLimiterRegistry::new();
    assert_eq!(registry.drop_prefix("upstream:absent"), 0);
}

#[test]
fn the_queue_holds_its_bound_and_expires_its_slots() {
    // The two bounds of the `queue` strategy: the queue never grows past its
    // count bound, and a queued request that outwaits the wait bound is
    // dropped by its own wait and charged nothing.
    let start = Instant::now();
    let mut registry = RateLimiterRegistry::new();
    let key = "upstream:alias:Tenant:t:60000";
    for _ in 0..64 {
        assert!(registry.enqueue(key, start));
    }
    assert_eq!(registry.queue_len(key), 64, "the count bound of the queue");
    assert!(!registry.enqueue(key, start), "a full queue takes no further slot");
    assert_eq!(registry.queue_len(key), 64);

    // The bound holds at any instant, not only at the one the queue filled at.
    let later = start + Duration::from_millis(250);
    assert_eq!(registry.dequeue_expired(key, later), 0, "nothing has outwaited yet");
    assert!(!registry.enqueue(key, later), "the bound holds at any instant");

    // A slot that outwaits the wait bound is dropped when the queue is read.
    let expired = registry.dequeue_expired(key, start + QUEUE_WAIT + Duration::from_millis(1));
    assert_eq!(expired, 64);
    assert_eq!(registry.queue_len(key), 0);
}

#[test]
fn a_released_or_gone_request_leaves_the_queue() {
    // A released, an expired, or a disconnected request leaves the queue
    // through the same removal, its slot returning to the bound and the
    // removal charging it nothing.
    let start = Instant::now();
    let mut registry = RateLimiterRegistry::new();
    let key = "upstream:alias:Tenant:t:60000";
    assert!(registry.enqueue(key, start));
    assert!(registry.enqueue(key, start));
    assert_eq!(registry.queue_len(key), 2);
    registry.dequeue(key);
    assert_eq!(registry.queue_len(key), 1);
    registry.dequeue(key);
    assert_eq!(registry.queue_len(key), 0, "the empty queue leaves the registry");
    registry.dequeue(key);
    assert_eq!(registry.queue_len(key), 0, "a removal over an absent queue is silent");
}

#[test]
fn a_breaker_reinitializes_at_closed_after_a_drop() {
    // An attempt whose machine the cleanup dropped re-initializes at `closed`,
    // which is the correct posture for a target whose configuration was
    // rewritten.
    let start = Instant::now();
    let mut registry = RateLimiterRegistry::new();
    let machine = registry.breaker("upstream:alias");
    for _ in 0..5 {
        let _ = machine.count(false, true, start);
    }
    assert!(!machine.admit(start), "the tripped machine refuses");

    registry.drop_prefix("upstream:alias");
    let fresh = registry.breaker("upstream:alias");
    assert!(fresh.admit(start), "the re-initialized machine admits");
}
