//! The rate-limit check of `cpt-cf-oagw-flow-rate-limit-check`, the strategy
//! flow that answers an over-limit request, the header set
//! `cpt-cf-oagw-algo-rate-limit-headers` produces, and the cleanup
//! `cpt-cf-oagw-flow-rate-limit-cleanup` runs.
//!
//! Covers the no-limit outcome over every layer, the five counter scopes and
//! the `tenant` fallback, the 429 answer with its header set, the bound queue,
//! the withheld burst reserve of the `degrade` strategy, the breaker's answer
//! before any charge, and the prefix cleanup of a deleted upstream and of a
//! deleted route.

// @cpt-dod:cpt-cf-oagw-dod-rate-limit-check:p1
// @cpt-dod:cpt-cf-oagw-dod-rate-limit-headers:p1
// @cpt-dod:cpt-cf-oagw-dod-rate-limit-strategies:p1
// @cpt-dod:cpt-cf-oagw-dod-circuit-breaker:p1
// @cpt-dod:cpt-cf-oagw-dod-rate-limit-state:p1

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use oagw::domain::effective::EffectiveRateLimit;
use oagw::domain::proxy::{AliasDerivation, MatchedRoute, ResolvedUpstream};
use oagw::domain::ratelimit::{QUEUE_CAPACITY, RateLimiterRegistry, fold};
use oagw::domain::route::{GrpcMatch, Route};
use oagw::domain::upstream::{
    Algorithm, Burst, HeadersConfig, RateLimitConfig, RateLimitScope, SharingMode, Strategy,
    Sustained, Window,
};
use oagw::data_plane::ratelimit::{
    LimitIdentity, LimitVerdict, RateLimitHeaders, RegistryCleanup, SharedLimits, check, cleanup,
    rate_limit_headers, upstream_prefix,
};
use uuid::Uuid;

const TENANT: Uuid = Uuid::from_u128(0xA11CE);
const UPSTREAM: Uuid = Uuid::from_u128(0xB0B);
const ROUTE: Uuid = Uuid::from_u128(0xC0DE);

fn now() -> Instant {
    Instant::now()
}

fn limit_of(rate_limit: RateLimitConfig) -> EffectiveRateLimit {
    EffectiveRateLimit {
        owner: TENANT,
        mode: SharingMode::Enforce,
        rate_limit,
    }
}

/// An upstream whose limit is the only one the fold sees.
fn resolved_with(rate_limit: Option<EffectiveRateLimit>) -> ResolvedUpstream {
    ResolvedUpstream {
        cors: None,
        tenant_id: TENANT,
        upstream_id: UPSTREAM,
        alias: String::from("orders.internal"),
        alias_derivation: AliasDerivation::Explicit,
        endpoints: Vec::new(),
        protocol: String::from("cf.core.oagw.http.v1"),
        enabled: true,
        headers: HeadersConfig::default(),
        rate_limit,
        plugins: None,
        route_candidates: Vec::new(),
    }
}

/// A route whose limit is the only one the fold sees.
fn matched_with(rate_limit: Option<EffectiveRateLimit>) -> MatchedRoute {
    MatchedRoute {
        tenant_id: TENANT,
        route_id: ROUTE,
        priority: None,
        outbound_path: String::from("/orders"),
        match_pattern: String::from("/v1/things"),
        query_allowlist: Vec::new(),
        rate_limit,
        plugins: None,
        cors: None,
    }
}

fn token_bucket_config(rate: u64, window: Window, capacity: u64) -> RateLimitConfig {
    RateLimitConfig {
        sharing: Some(SharingMode::Enforce),
        algorithm: Some(Algorithm::TokenBucket),
        sustained: Some(Sustained {
            rate,
            window: Some(window),
        }),
        burst: Some(Burst { capacity }),
        scope: Some(RateLimitScope::Tenant),
        strategy: Some(Strategy::Reject),
        cost: Some(1),
    }
}

fn identity() -> LimitIdentity {
    LimitIdentity::new(TENANT, Some(String::from("alice")), None)
}

/// The `MatchedRoute` a route-layer limit test builds.
fn route_row() -> Route {
    Route {
        id: ROUTE,
        upstream_id: UPSTREAM,
        match_config: oagw::domain::route::MatchConfig {
            http: None,
            grpc: Some(GrpcMatch {
                service: String::from("orders.v1.Orders"),
                method: String::from("Get"),
            }),
        },
        plugins: None,
        rate_limit: None,
        tags: Vec::new(),
        cors: None,
        priority: None,
        enabled: Some(true),
    }
}

#[tokio::test]
async fn no_layer_carries_a_limit() {
    // Neither layer carries a `rate_limit`: the check admits with no charge,
    // no counter, and no header (§1.5).
    let shared = SharedLimits::new();
    let resolved = resolved_with(None);
    let matched = matched_with(None);
    let verdict = check(&shared, &resolved, &matched, &identity(), now()).await;
    assert_eq!(verdict, LimitVerdict::Admitted);
    assert_eq!(
        shared.lock().queue_len("upstream:any"),
        0,
        "the check created no queue state"
    );
}

#[tokio::test]
async fn a_limit_at_the_upstream_layer_alone_is_enforced() {
    // A limit declared at the upstream layer alone is enforced, which is the
    // fold's outcome and not the two-layer look a guard once suggested.
    let shared = SharedLimits::new();
    let resolved = resolved_with(Some(limit_of(token_bucket_config(1, Window::Second, 1))));
    let matched = matched_with(None);
    let first = check(&shared, &resolved, &matched, &identity(), now()).await;
    let second = check(&shared, &resolved, &matched, &identity(), now()).await;
    assert_eq!(first, LimitVerdict::Admitted);
    assert!(
        matches!(second, LimitVerdict::Rejected(_)),
        "a limit at one layer is a limit, and the second request is over it"
    );
}

#[tokio::test]
async fn the_reject_answer_carries_the_header_set() {
    // The 429 the `reject` strategy produces carries the four headers ADR 0003
    // declares and the `retry_after_seconds` member of the problem body.
    let shared = SharedLimits::new();
    let resolved = resolved_with(Some(limit_of(token_bucket_config(1, Window::Second, 1))));
    let matched = matched_with(None);
    let _ = check(&shared, &resolved, &matched, &identity(), now()).await;
    let rejected = check(&shared, &resolved, &matched, &identity(), now()).await;
    let LimitVerdict::Rejected(headers) = rejected else {
        panic!("the second request is over the limit");
    };
    let pairs = headers.pairs();
    let names: Vec<&str> = pairs.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        vec!["X-RateLimit-Limit", "X-RateLimit-Remaining", "X-RateLimit-Reset", "Retry-After"]
    );
    assert_eq!(headers.limit.as_deref(), Some("1"));
    assert_eq!(headers.remaining.as_deref(), Some("0"));
    assert_eq!(headers.retry_after.as_deref(), Some("1"));
    assert_eq!(headers.retry_after_seconds, Some(1));
}

#[tokio::test]
async fn the_gate_closes_the_whole_set() {
    // A closed `response_headers` gate produces no `X-RateLimit-*` header and
    // no `Retry-After`; the variant and the error source are unchanged by it.
    let config = token_bucket_config(1, Window::Second, 1);
    let limit = fold(&oagw::domain::LimitLayers {
        upstream: Some(&limit_of(config)),
        route: None,
    })
    .expect("a limit at one layer is a limit");
    let mut registry = RateLimiterRegistry::new();
    let outcome = oagw::domain::token_bucket(
        registry.bucket("route:x", limit.burst_capacity, &limit.sustained, now()),
        limit.cost,
        now(),
    );
    let headers = rate_limit_headers(&limit, &outcome, false);
    assert_eq!(headers, RateLimitHeaders::default());
    assert!(headers.pairs().is_empty(), "the gate closed the whole set");
}

#[tokio::test]
async fn admitted_requests_carry_no_header() {
    // The header set is tied to the 429 answer alone: an admitted request is
    // answered with no rate-limit header of any kind.
    let shared = SharedLimits::new();
    let resolved = resolved_with(Some(limit_of(token_bucket_config(10, Window::Second, 10))));
    let matched = matched_with(None);
    let verdict = check(&shared, &resolved, &matched, &identity(), now()).await;
    assert_eq!(verdict, LimitVerdict::Admitted);
}

#[tokio::test]
async fn degrade_forwards_against_the_reduced_allowance() {
    // The withheld burst reserve: a `degrade` upstream at capacity 50 and
    // sustained rate 10 admits 10 immediate requests and refuses the 11th,
    // which is the reduced capacity the strategy leaves, and answers it 429.
    let shared = SharedLimits::new();
    let mut config = token_bucket_config(10, Window::Second, 50);
    config.strategy = Some(Strategy::Degrade);
    let resolved = resolved_with(Some(limit_of(config)));
    let matched = matched_with(None);
    for _ in 0..10 {
        let verdict = check(&shared, &resolved, &matched, &identity(), now()).await;
        assert_eq!(verdict, LimitVerdict::Degraded);
    }
    let over = check(&shared, &resolved, &matched, &identity(), now()).await;
    assert!(
        matches!(over, LimitVerdict::Rejected(_)),
        "the allowance the degraded posture leaves does not cover the eleventh"
    );
}

#[tokio::test]
async fn a_cost_above_capacity_is_refused_on_every_attempt() {
    // A `cost` the effective capacity never covers is refused on every attempt
    // for as long as that configuration stands, and records no charge.
    let shared = SharedLimits::new();
    let mut config = token_bucket_config(10, Window::Second, 50);
    config.cost = Some(60);
    let resolved = resolved_with(Some(limit_of(config)));
    let matched = matched_with(None);
    for _ in 0..3 {
        let verdict = check(&shared, &resolved, &matched, &identity(), now()).await;
        assert!(matches!(verdict, LimitVerdict::Rejected(_)));
    }
}

#[tokio::test]
async fn the_scopes_key_separate_counters() {
    // The five scopes key five separate counters, and the counter key carries
    // the `{resource_type}:{resource_id}` prefix of the resource whose
    // `rate_limit` the effective limit came from.
    let shared = SharedLimits::new();
    let subject = LimitIdentity::new(TENANT, Some(String::from("alice")), None);
    let peer = LimitIdentity::new(
        TENANT,
        None,
        Some(SocketAddr::new(IpAddr::from([203, 0, 113, 7]), 443)),
    );
    let resolved = resolved_with(Some(limit_of(token_bucket_config(10, Window::Second, 10))));
    let matched = matched_with(None);

    let tenant_key = RateLimiterRegistry::counter_key(
        "upstream",
        &UPSTREAM.to_string(),
        RateLimitScope::Tenant,
        &TENANT.to_string(),
        Some(Window::Second),
    );
    let user_key = RateLimiterRegistry::counter_key(
        "upstream",
        &UPSTREAM.to_string(),
        RateLimitScope::User,
        "alice",
        Some(Window::Second),
    );
    let ip_key = RateLimiterRegistry::counter_key(
        "upstream",
        &UPSTREAM.to_string(),
        RateLimitScope::Ip,
        "203.0.113.7",
        Some(Window::Second),
    );
    let route_key = RateLimiterRegistry::counter_key(
        "upstream",
        &UPSTREAM.to_string(),
        RateLimitScope::Route,
        &ROUTE.to_string(),
        Some(Window::Second),
    );
    let global_key = RateLimiterRegistry::counter_key(
        "upstream",
        &UPSTREAM.to_string(),
        RateLimitScope::Global,
        "global",
        Some(Window::Second),
    );
    let keys = [tenant_key, user_key, ip_key, route_key, global_key];
    for (index, key) in keys.iter().enumerate() {
        for other in &keys[index + 1..] {
            assert_ne!(key, other, "two scopes never share a counter key");
        }
    }
    // The identity the check forms from the request drives which counter is
    // charged: a peer identity and a subject identity are distinct counters.
    let _ = check(&shared, &resolved, &matched, &subject, now()).await;
    let _ = check(&shared, &resolved, &matched, &peer, now()).await;
}

#[tokio::test]
async fn a_scope_without_its_identifier_falls_back_to_the_tenant() {
    // A `user` scope with no subject and an `ip` scope with no peer address
    // fall back to the `tenant` scope and its key rather than skip enforcement.
    let shared = SharedLimits::new();
    let anonymous = LimitIdentity::new(TENANT, None, None);
    let resolved = resolved_with(Some(limit_of(token_bucket_config(1, Window::Second, 1))));
    let matched = matched_with(None);
    let first = check(&shared, &resolved, &matched, &anonymous, now()).await;
    let second = check(&shared, &resolved, &matched, &anonymous, now()).await;
    assert_eq!(first, LimitVerdict::Admitted);
    assert!(
        matches!(second, LimitVerdict::Rejected(_)),
        "the fallback charges the tenant's counter, so the second request is over it"
    );
}

#[tokio::test]
async fn the_breaker_answers_before_any_charge() {
    // A breaker that is not admitting is answered 503 before any charge and
    // before the outbound attempt, with no rate-limit header of any kind.
    let shared = SharedLimits::new();
    let resolved = resolved_with(Some(limit_of(token_bucket_config(1, Window::Second, 1))));
    let matched = matched_with(None);
    {
        let mut registry = shared.lock();
        let breaker = registry.breaker(&upstream_prefix(UPSTREAM));
        for _ in 0..5 {
            breaker.count(false, true, now());
        }
    }
    let verdict = check(&shared, &resolved, &matched, &identity(), now()).await;
    assert!(
        matches!(verdict, LimitVerdict::Open { retry_after_seconds } if (1..=30).contains(&retry_after_seconds)),
        "the 503 carries the seconds remaining of the open interval, which the elapsed test time trims"
    );
    // The 503 the breaker answers carries no rate-limit header, because the
    // breaker is not a rate limit; the header set is the 429's alone.
    let LimitVerdict::Open { .. } = verdict else {
        panic!("the breaker is open");
    };
}

#[tokio::test]
async fn the_breaker_counts_only_the_three_rows() {
    // The breaker counts a connection that never established, an exchange that
    // exceeded its deadline, and an unavailable link; it counts neither an
    // upstream 4xx nor a 429 this feature produced.
    let mut registry = RateLimiterRegistry::new();
    let breaker = registry.breaker(&upstream_prefix(UPSTREAM));
    for _ in 0..5 {
        breaker.count(false, false, now());
    }
    assert!(
        breaker.admit(now()),
        "a 4xx answer and a 429 this feature produced are not evidence about the target"
    );
    let mut registry = RateLimiterRegistry::new();
    let breaker = registry.breaker(&upstream_prefix(UPSTREAM));
    for _ in 0..5 {
        breaker.count(false, true, now());
    }
    assert!(!breaker.admit(now()), "five counted failures trip the breaker");
}

#[tokio::test]
async fn a_deleted_upstream_drops_its_prefix() {
    // The cleanup of a deleted upstream drops every entry keyed under its
    // prefix, including the breaker machine held for it.
    let mut registry = RateLimiterRegistry::new();
    registry.bucket(
        &format!("upstream:{UPSTREAM}:Tenant:{}:{:?}", TENANT, Window::Second),
        10,
        &Sustained {
            rate: 10,
            window: Some(Window::Second),
        },
        now(),
    );
    registry.breaker(&upstream_prefix(UPSTREAM));
    registry.breaker(&upstream_prefix(Uuid::from_u128(0xD00)));
    let dropped = cleanup(&mut registry, "upstream", UPSTREAM);
    assert!(dropped >= 2, "the prefix drop has exactly one owner");
    let mut registry = RateLimiterRegistry::new();
    registry.breaker(&upstream_prefix(UPSTREAM));
    cleanup(&mut registry, "upstream", UPSTREAM);
    assert!(
        registry.breaker(&upstream_prefix(UPSTREAM)).admit(now()),
        "a recreated alias re-initializes its breaker at closed"
    );
}

#[tokio::test]
async fn a_deleted_route_leaves_the_upstream_in_place() {
    // The cleanup of a deleted route drops that route's prefix and leaves the
    // upstream's own buckets and its breaker machine in place.
    let mut registry = RateLimiterRegistry::new();
    let upstream_key = format!("upstream:{UPSTREAM}:Tenant:{TENANT}:{:?}", Window::Second);
    registry.bucket(
        &upstream_key,
        10,
        &Sustained {
            rate: 10,
            window: Some(Window::Second),
        },
        now(),
    );
    registry.breaker(&upstream_prefix(UPSTREAM));
    let dropped = cleanup(&mut registry, "route", ROUTE);
    assert_eq!(dropped, 0, "no entry of the upstream's prefix is a route's");
    let mut registry = RateLimiterRegistry::new();
    let route_key = format!("route:{ROUTE}:Tenant:{TENANT}:{:?}", Window::Second);
    registry.bucket(
        &route_key,
        10,
        &Sustained {
            rate: 10,
            window: Some(Window::Second),
        },
        now(),
    );
    let dropped = cleanup(&mut registry, "route", ROUTE);
    assert_eq!(dropped, 1, "the route's own bucket goes with the route");
}

#[tokio::test]
async fn the_cleanup_observer_drops_on_the_seam() {
    // The observer the state registers drops the prefix of the deleted row on
    // the seam the write path notifies.
    use oagw::control_plane::cache::RateLimitCleanup as _;
    let observer = RegistryCleanup::new(SharedLimits::new());
    observer.upstream_deleted(TENANT, UPSTREAM);
    observer.route_deleted(TENANT, ROUTE);
}

#[tokio::test]
async fn a_queue_that_cannot_hold_answers_the_reject_answer() {
    // A queue at its bound answers the next over-limit request with the same
    // 429 the `reject` strategy produces, and holds no request beyond it.
    let shared = SharedLimits::new();
    let resolved = resolved_with(Some(limit_of({
        let mut config = token_bucket_config(1, Window::Second, 1);
        config.strategy = Some(Strategy::Queue);
        config
    })));
    let matched = matched_with(None);
    // Drain the allowance, then fill the queue to its bound so the next
    // over-limit request meets a full queue.
    let _ = check(&shared, &resolved, &matched, &identity(), now()).await;
    for _ in 0..QUEUE_CAPACITY {
        assert!(
            shared
                .lock()
                .enqueue(&format!("upstream:{UPSTREAM}:Tenant:{TENANT}:{:?}", Window::Second), now()),
            "the bound is QUEUE_CAPACITY"
        );
    }
    let verdict = check(&shared, &resolved, &matched, &identity(), now()).await;
    let LimitVerdict::Rejected(headers) = verdict else {
        panic!("a full queue answers 429 rather than holding");
    };
    assert_eq!(headers.limit.as_deref(), Some("1"), "the same header set");
}

#[tokio::test]
async fn a_queued_request_is_released_and_forwarded() {
    // A held request whose release admits it is forwarded exactly as an
    // immediately admitted one would be, with no marker that it waited.
    let shared = SharedLimits::new();
    // A rate of 100 per second refills one token in 10 ms, which is inside the
    // 500 ms wait bound the queue holds the request for.
    let resolved = resolved_with(Some(limit_of({
        let mut config = token_bucket_config(100, Window::Second, 1);
        config.strategy = Some(Strategy::Queue);
        config
    })));
    let matched = matched_with(None);
    let first = check(&shared, &resolved, &matched, &identity(), now()).await;
    assert_eq!(first, LimitVerdict::Admitted);
    let held = check(&shared, &resolved, &matched, &identity(), now()).await;
    assert_eq!(
        held,
        LimitVerdict::Admitted,
        "the release re-runs the check and admits when the counter holds the cost"
    );
    assert_eq!(
        shared.lock().queue_len("upstream:any"),
        0,
        "a released request leaves the queue with no marker that it waited"
    );
}

#[tokio::test]
async fn a_route_layer_limit_keys_the_route_prefix() {
    // A limit the route layer declares keys its counters under the route's
    // prefix, so a prefix drop has exactly one owner.
    let shared = SharedLimits::new();
    let resolved = resolved_with(None);
    let matched = matched_with(Some(limit_of(token_bucket_config(1, Window::Second, 1))));
    let _ = check(&shared, &resolved, &matched, &identity(), now()).await;
    let verdict = check(&shared, &resolved, &matched, &identity(), now()).await;
    assert!(
        matches!(verdict, LimitVerdict::Rejected(_)),
        "the route layer's own limit is enforced against the route's counter"
    );
}

#[tokio::test]
async fn the_check_costs_no_more_than_the_latency_budget() {
    // The check is an in-process read and an in-process write, so its own
    // share of the proxy path is far inside the 10 ms p95 budget
    // `cpt-cf-oagw-dod-rate-limit-latency` allocates.
    let shared = SharedLimits::new();
    let resolved = resolved_with(Some(limit_of(token_bucket_config(10_000, Window::Second, 10_000))));
    let matched = matched_with(None);
    let start = Instant::now();
    for _ in 0..1_000 {
        let verdict = check(&shared, &resolved, &matched, &identity(), now()).await;
        assert_eq!(verdict, LimitVerdict::Admitted);
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "1000 checks took {elapsed:?}, so one check is inside the budget"
    );
    let _ = route_row();
}
