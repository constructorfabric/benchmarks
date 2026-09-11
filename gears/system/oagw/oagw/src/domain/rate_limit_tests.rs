//! Tests for the rate limiter (ADR-0003): the two algorithms, the scoping key,
//! the effective-limit resolution and the 429 problem document with its headers.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::json;
use uuid::Uuid;

use crate::domain::model::{RateLimitConfig, RateLimitScope, RateLimitWindow};
use crate::error::{
    RATE_LIMIT_EXCEEDED_TYPE, RETRY_AFTER_HEADER, X_RATELIMIT_LIMIT_HEADER,
    X_RATELIMIT_REMAINING_HEADER, X_RATELIMIT_RESET_HEADER,
};

use super::{
    EffectiveLimit, MAX_COUNTERS, MonotonicClock, RateLimitClock, RateLimitRule, RateLimitSubject,
    RateLimiter, scope_key,
};

// ── Fixtures ─────────────────────────────────────────────────────────────────

/// A clock the test moves by hand, so refill and window behaviour are
/// deterministic instead of racing the scheduler.
#[derive(Debug, Clone)]
struct SteppedClock {
    /// Fixed at construction: every `now` is the same base plus the offset the
    /// test moved to, so the durations the limiter computes are exact.
    base: Instant,
    millis: Arc<AtomicU64>,
}

impl SteppedClock {
    fn advance(&self, millis: u64) {
        self.millis.fetch_add(millis, Ordering::Relaxed);
    }

    fn elapsed(&self) -> Duration {
        Duration::from_millis(self.millis.load(Ordering::Relaxed))
    }
}

impl RateLimitClock for SteppedClock {
    fn now(&self) -> Instant {
        self.base + self.elapsed()
    }
}

fn limiter() -> (RateLimiter, SteppedClock) {
    let clock = SteppedClock {
        base: Instant::now(),
        millis: Arc::new(AtomicU64::new(0)),
    };
    (RateLimiter::with_clock(Arc::new(clock.clone())), clock)
}

fn document(document: serde_json::Value) -> RateLimitConfig {
    serde_json::from_value(document).expect("rate limit document")
}

fn subject() -> RateLimitSubject {
    RateLimitSubject {
        tenant_id: Uuid::from_u128(0xA11A),
        subject_id: Some(Uuid::from_u128(0xFEED)),
        peer: None,
        route_id: Uuid::from_u128(0x7E57),
    }
}

// ── Counter map bounding ─────────────────────────────────────────────────────

#[test]
fn a_map_full_of_live_counters_evicts_the_least_recently_active_one() {
    let (limiter, clock) = limiter();
    let config = document(json!({ "sustained": { "rate": 2, "window": "minute" }, "scope": "ip" }));
    let rule = RateLimitRule::upstream(Uuid::from_u128(0x1), &config);
    // The scope the test later revisits: exhausted first, so it is the oldest
    // live counter the sweep can pick.
    let first = RateLimitSubject {
        peer: Some("10.0.0.1".parse().unwrap()),
        ..subject()
    };
    for _ in 0..2 {
        assert!(limiter.enforce(std::slice::from_ref(&rule), &first).is_ok());
    }
    assert!(
        limiter
            .enforce(std::slice::from_ref(&rule), &first)
            .is_err()
    );

    // Fill the map with fresh, live scopes until the cap is reached.
    for index in 1..=MAX_COUNTERS {
        let peer = IpAddr::V4(Ipv4Addr::from(u32::try_from(index).unwrap_or(1)));
        let other = RateLimitSubject {
            peer: Some(peer),
            ..subject()
        };
        let _ignored = limiter.enforce(std::slice::from_ref(&rule), &other);
        clock.advance(1);
    }
    assert!(
        limiter.counters.lock().len() < MAX_COUNTERS + 1,
        "the sweep must keep the map bounded"
    );

    // The first scope was evicted, so it restarts full: the request its
    // exhausted budget used to refuse is admitted again.
    assert!(
        limiter.enforce(&[rule], &first).is_ok(),
        "an evicted counter restarts full"
    );
}

#[test]
fn an_idle_counter_is_swept_before_a_live_one_is_evicted() {
    let (limiter, clock) = limiter();
    let config = document(json!({ "sustained": { "rate": 1, "window": "minute" }, "scope": "ip" }));
    let rule = RateLimitRule::upstream(Uuid::from_u128(0x1), &config);

    // The counter the test later abandons: hit once, never again.
    let idle = RateLimitSubject {
        peer: Some("10.0.0.1".parse().unwrap()),
        ..subject()
    };
    let idle_key = rule.key(&idle);
    let _ignored = limiter.enforce(std::slice::from_ref(&rule), &idle);

    // Past its window the counter is dead weight; the fill below drives the map
    // to the cap, and the next distinct scope triggers the sweep.
    clock.advance(u64::try_from(Duration::from_secs(61).as_millis()).unwrap_or(0));
    let mut first_live_key = None;
    for index in 1..MAX_COUNTERS {
        let peer = IpAddr::V4(Ipv4Addr::from(u32::try_from(index).unwrap_or(1)));
        let live = RateLimitSubject {
            peer: Some(peer),
            ..subject()
        };
        if first_live_key.is_none() {
            first_live_key = Some(rule.key(&live));
        }
        let _ignored = limiter.enforce(std::slice::from_ref(&rule), &live);
    }
    assert_eq!(limiter.counters.lock().len(), MAX_COUNTERS);

    // One more distinct scope: the sweep runs, drops the dead counter and keeps
    // every live one — including the oldest live one.
    let extra = RateLimitSubject {
        peer: Some("10.0.0.3".parse().unwrap()),
        ..subject()
    };
    let _ignored = limiter.enforce(std::slice::from_ref(&rule), &extra);

    let counters = limiter.counters.lock();
    assert!(
        !counters.contains_key(idle_key.as_str()),
        "the idle counter is gone"
    );
    assert!(
        counters.contains_key(first_live_key.as_deref().unwrap()),
        "the oldest live counter survives"
    );
    assert!(counters.len() <= MAX_COUNTERS);
}

// ── Token bucket ─────────────────────────────────────────────────────────────

#[test]
fn a_token_bucket_allows_a_burst_up_to_the_capacity_and_then_rejects() {
    let (limiter, _clock) = limiter();
    let config = document(json!({ "sustained": { "rate": 5, "window": "minute" } }));
    let rule = RateLimitRule::upstream(Uuid::from_u128(0x1), &config);
    // No `burst` member: the capacity defaults to the sustained rate.
    let effective = EffectiveLimit::of(&config);
    assert_eq!(effective.capacity, 5);
    let key = rule.key(&subject());

    for _ in 0..5 {
        assert!(
            limiter.check(key.as_str(), &effective).is_allowed(),
            "the first `capacity` requests are admitted"
        );
    }
    assert!(
        limiter.check(key.as_str(), &effective).is_rejected(),
        "the burst is exhausted"
    );
}

#[test]
fn a_token_bucket_refills_at_the_sustained_rate() {
    let (limiter, clock) = limiter();
    let config = document(json!({ "sustained": { "rate": 60, "window": "minute" } }));
    let effective = EffectiveLimit::of(&config);
    let key = RateLimitRule::upstream(Uuid::from_u128(0x1), &config).key(&subject());

    for _ in 0..60 {
        limiter
            .check(key.as_str(), &effective)
            .allowed()
            .expect("full bucket");
    }
    assert!(limiter.check(key.as_str(), &effective).is_rejected());

    // One token per second: after 10 s exactly 10 tokens are available again.
    clock.advance(10_000);
    for _ in 0..10 {
        limiter
            .check(key.as_str(), &effective)
            .allowed()
            .expect("refilled tokens are spendable");
    }
    assert!(
        limiter.check(key.as_str(), &effective).is_rejected(),
        "the refill must not over-issue tokens"
    );
}

#[test]
fn a_token_bucket_refills_only_up_to_its_capacity() {
    let (limiter, clock) = limiter();
    let config = document(json!({
        "sustained": { "rate": 10, "window": "second" },
        "burst": { "capacity": 3 }
    }));
    let effective = EffectiveLimit::of(&config);
    let key = RateLimitRule::upstream(Uuid::from_u128(0x1), &config).key(&subject());

    clock.advance(60_000);
    for _ in 0..3 {
        limiter
            .check(key.as_str(), &effective)
            .allowed()
            .expect("the bucket holds at most `capacity` tokens");
    }
    assert!(
        limiter.check(key.as_str(), &effective).is_rejected(),
        "an idle bucket must not grow past its capacity"
    );
}

#[test]
fn the_bucket_rejection_reports_when_a_retry_can_succeed() {
    let (limiter, _clock) = limiter();
    let config = document(json!({
        "sustained": { "rate": 60, "window": "minute" },
        "burst": { "capacity": 1 }
    }));
    let effective = EffectiveLimit::of(&config);
    let key = RateLimitRule::upstream(Uuid::from_u128(0x1), &config).key(&subject());

    limiter
        .check(key.as_str(), &effective)
        .allowed()
        .expect("admitted");
    let rejection = limiter
        .check(key.as_str(), &effective)
        .allowed()
        .expect_err("the bucket is empty");
    // One token per second and a cost of one: the next token arrives within 1 s.
    assert_eq!(rejection.retry_after, Duration::from_secs(1));
    assert_eq!(rejection.remaining, 0);
    assert_eq!(rejection.limit, 60);
}

// ── Sliding window ───────────────────────────────────────────────────────────

#[test]
fn a_sliding_window_rejects_past_the_rate_within_the_window() {
    let (limiter, _clock) = limiter();
    let config = document(json!({
        "algorithm": "sliding_window",
        "sustained": { "rate": 3, "window": "second" }
    }));
    let effective = EffectiveLimit::of(&config);
    let key = RateLimitRule::upstream(Uuid::from_u128(0x1), &config).key(&subject());

    for _ in 0..3 {
        limiter
            .check(key.as_str(), &effective)
            .allowed()
            .expect("within the rate");
    }
    assert!(
        limiter.check(key.as_str(), &effective).is_rejected(),
        "a fourth request inside the same window is refused"
    );
}

#[test]
fn a_sliding_window_frees_the_slots_as_they_age_out() {
    let (limiter, clock) = limiter();
    let config = document(json!({
        "algorithm": "sliding_window",
        "sustained": { "rate": 2, "window": "second" }
    }));
    let effective = EffectiveLimit::of(&config);
    let key = RateLimitRule::upstream(Uuid::from_u128(0x1), &config).key(&subject());

    limiter
        .check(key.as_str(), &effective)
        .allowed()
        .expect("first");
    clock.advance(400);
    limiter
        .check(key.as_str(), &effective)
        .allowed()
        .expect("second");
    assert!(limiter.check(key.as_str(), &effective).is_rejected());

    // The first acquisition leaves the window after 1 s, freeing one slot.
    clock.advance(600);
    limiter
        .check(key.as_str(), &effective)
        .allowed()
        .expect("the oldest acquisition aged out");
}

#[test]
fn a_sliding_window_rejection_waits_for_the_oldest_acquisition() {
    let (limiter, clock) = limiter();
    let config = document(json!({
        "algorithm": "sliding_window",
        "sustained": { "rate": 1, "window": "second" }
    }));
    let effective = EffectiveLimit::of(&config);
    let key = RateLimitRule::upstream(Uuid::from_u128(0x1), &config).key(&subject());

    limiter
        .check(key.as_str(), &effective)
        .allowed()
        .expect("first");
    clock.advance(250);
    let rejection = limiter
        .check(key.as_str(), &effective)
        .allowed()
        .expect_err("the window is full");
    assert_eq!(rejection.retry_after, Duration::from_millis(750));
}

// ── Scoping ──────────────────────────────────────────────────────────────────

/// The window unit every scoping assertion is made under.
const WINDOW: RateLimitWindow = RateLimitWindow::Minute;

#[test]
fn the_scope_key_follows_the_adr_key_layout() {
    let subject = RateLimitSubject {
        tenant_id: Uuid::from_u128(0xA11A),
        subject_id: Some(Uuid::from_u128(0xFEED)),
        peer: Some("203.0.113.7".parse().expect("peer address")),
        route_id: Uuid::from_u128(0x7E57),
    };
    assert_eq!(
        scope_key("upstream", "1111", RateLimitScope::Tenant, &subject, WINDOW),
        format!(
            "oagw:ratelimit:upstream:1111:tenant:{}:minute",
            Uuid::from_u128(0xA11A)
        )
    );
    assert_eq!(
        scope_key("route", "2222", RateLimitScope::Route, &subject, WINDOW),
        format!(
            "oagw:ratelimit:route:2222:route:{}:minute",
            Uuid::from_u128(0x7E57)
        )
    );
    assert_eq!(
        scope_key("upstream", "1111", RateLimitScope::Global, &subject, WINDOW),
        "oagw:ratelimit:upstream:1111:global:-:minute"
    );
    assert_eq!(
        scope_key("upstream", "1111", RateLimitScope::User, &subject, WINDOW),
        format!(
            "oagw:ratelimit:upstream:1111:user:{}:minute",
            Uuid::from_u128(0xFEED)
        )
    );
    assert_eq!(
        scope_key("upstream", "1111", RateLimitScope::Ip, &subject, WINDOW),
        "oagw:ratelimit:upstream:1111:ip:203.0.113.7:minute"
    );
}

#[test]
fn every_scope_gets_its_own_counter() {
    let (limiter, _clock) = limiter();
    let config = document(json!({ "sustained": { "rate": 1, "window": "minute" } }));
    let effective = EffectiveLimit::of(&config);
    let rule = RateLimitRule::upstream(Uuid::from_u128(0x1), &config);

    let tenant_subject = RateLimitSubject {
        subject_id: None,
        ..subject()
    };
    let other_tenant = RateLimitSubject {
        tenant_id: Uuid::from_u128(0xB22B),
        ..tenant_subject
    };

    limiter
        .check(rule.key(&tenant_subject).as_str(), &effective)
        .allowed()
        .expect("the tenant budget is spent");
    assert!(
        limiter
            .check(rule.key(&tenant_subject).as_str(), &effective)
            .is_rejected()
    );
    limiter
        .check(rule.key(&other_tenant).as_str(), &effective)
        .allowed()
        .expect("another tenant has its own counter");
}

#[test]
fn a_user_scope_without_a_subject_degrades_to_the_tenant_counter() {
    let anonymous = RateLimitSubject {
        subject_id: None,
        ..subject()
    };
    assert_eq!(
        scope_key("upstream", "1", RateLimitScope::User, &anonymous, WINDOW),
        format!(
            "oagw:ratelimit:upstream:1:user:{}:minute",
            anonymous.tenant_id
        ),
        "an unauthenticated caller cannot open a shared, keyless counter"
    );
}

#[test]
fn an_ip_scope_without_a_peer_degrades_to_the_tenant_counter() {
    let unroutable = RateLimitSubject {
        peer: None,
        ..subject()
    };
    assert_eq!(
        scope_key("upstream", "1", RateLimitScope::Ip, &unroutable, WINDOW),
        format!(
            "oagw:ratelimit:upstream:1:ip:{}:minute",
            unroutable.tenant_id
        )
    );
}

// ── Effective limits ─────────────────────────────────────────────────────────

#[test]
fn both_the_upstream_and_the_route_limit_must_allow() {
    let (limiter, _clock) = limiter();
    let loose = document(json!({ "sustained": { "rate": 10, "window": "minute" } }));
    let strict = document(json!({ "sustained": { "rate": 1, "window": "minute" } }));

    let rules = vec![
        RateLimitRule::upstream(Uuid::from_u128(0x1), &loose),
        RateLimitRule::route(Uuid::from_u128(0x2), &strict),
    ];
    let subject = subject();

    limiter
        .enforce(&rules, &subject)
        .expect("both counters admit the request");

    // The upstream limit alone still has room, so the route limit is what binds.
    assert!(
        limiter
            .check(rules[0].key(&subject).as_str(), rules[0].limit())
            .is_allowed()
    );

    let rejection = limiter
        .enforce(&rules, &subject)
        .expect_err("the route counter is exhausted");
    assert_eq!(rejection.status_code(), 429);
    assert_eq!(rejection.gts_type(), RATE_LIMIT_EXCEEDED_TYPE);
}

#[test]
fn the_cost_is_charged_per_request() {
    let (limiter, _clock) = limiter();
    let config = document(json!({
        "sustained": { "rate": 10, "window": "minute" },
        "cost": 4
    }));
    let effective = EffectiveLimit::of(&config);
    assert_eq!(effective.cost, 4);
    let key = RateLimitRule::upstream(Uuid::from_u128(0x1), &config).key(&subject());

    limiter
        .check(key.as_str(), &effective)
        .allowed()
        .expect("first");
    limiter
        .check(key.as_str(), &effective)
        .allowed()
        .expect("second");
    let rejection = limiter
        .check(key.as_str(), &effective)
        .allowed()
        .expect_err("4 + 4 leaves 2 tokens, the third request costs 4");
    assert_eq!(rejection.remaining, 2);
}

#[test]
fn the_window_unit_is_read_from_the_document() {
    let day = document(json!({ "sustained": { "rate": 5, "window": "day" } }));
    assert_eq!(
        EffectiveLimit::of(&day).window_length(),
        Duration::from_hours(24)
    );

    let hour = document(json!({ "sustained": { "rate": 5, "window": "hour" } }));
    assert_eq!(
        EffectiveLimit::of(&hour).window_length(),
        Duration::from_hours(1)
    );
}

// ── 429 problem document ─────────────────────────────────────────────────────

#[test]
fn the_rejection_is_a_gateway_problem_document_with_the_rate_limit_headers() {
    let (limiter, _clock) = limiter();
    let config = document(json!({
        "sustained": { "rate": 60, "window": "minute" },
        "burst": { "capacity": 2 }
    }));
    let rule = RateLimitRule::upstream(Uuid::from_u128(0x1), &config);
    let subject = subject();
    let key = rule.key(&subject);

    limiter
        .check(key.as_str(), &EffectiveLimit::of(&config))
        .allowed()
        .expect("first");
    limiter
        .check(key.as_str(), &EffectiveLimit::of(&config))
        .allowed()
        .expect("second");

    let error = limiter
        .enforce(&[rule], &subject)
        .expect_err("the bucket is exhausted");
    assert_eq!(error.status_code(), 429);
    assert_eq!(error.gts_type(), RATE_LIMIT_EXCEEDED_TYPE);
    assert_eq!(error.extensions().retry_after_seconds, Some(1));
    assert_eq!(
        error.detail(),
        "the rate limit of 60 tokens per minute for the upstream is exhausted: 2 of them may be \
         spent at once and the request costs 1"
    );

    let headers = error.extra_headers();
    assert_eq!(
        headers
            .iter()
            .find(|(name, _)| *name == RETRY_AFTER_HEADER)
            .map(|(_, v)| v),
        Some(&"1".parse().unwrap())
    );
    assert_eq!(
        headers
            .iter()
            .find(|(name, _)| *name == X_RATELIMIT_LIMIT_HEADER)
            .map(|(_, v)| v),
        Some(&"60".parse().unwrap())
    );
    assert_eq!(
        headers
            .iter()
            .find(|(name, _)| *name == X_RATELIMIT_REMAINING_HEADER)
            .map(|(_, v)| v),
        Some(&"0".parse().unwrap())
    );
    assert!(
        headers
            .iter()
            .any(|(name, _)| *name == X_RATELIMIT_RESET_HEADER)
    );
}

#[test]
fn the_sliding_window_rejection_reports_the_window_budget() {
    let (limiter, clock) = limiter();
    let config = document(json!({
        "algorithm": "sliding_window",
        "sustained": { "rate": 2, "window": "minute" }
    }));
    let effective = EffectiveLimit::of(&config);
    let key = RateLimitRule::upstream(Uuid::from_u128(0x1), &config).key(&subject());

    limiter
        .check(key.as_str(), &effective)
        .allowed()
        .expect("first");
    limiter
        .check(key.as_str(), &effective)
        .allowed()
        .expect("second");
    let rejection = limiter
        .check(key.as_str(), &effective)
        .allowed()
        .expect_err("the window holds two requests");
    assert_eq!(rejection.remaining, 0);
    assert_eq!(rejection.limit, 2);
    clock.advance(1_000);
    assert_eq!(
        limiter
            .check(key.as_str(), &effective)
            .allowed()
            .expect_err("still inside the window")
            .retry_after,
        Duration::from_secs(59)
    );
}

// ── Clock behaviour ──────────────────────────────────────────────────────────

#[test]
fn the_monotonic_clock_reports_the_real_time() {
    let first = MonotonicClock.now();
    let second = MonotonicClock.now();
    assert!(second >= first, "a monotonic clock never goes backwards");
}

#[test]
fn an_old_counter_is_rebuilt_from_the_current_configuration() {
    let (limiter, clock) = limiter();
    let config = document(json!({ "sustained": { "rate": 1, "window": "second" } }));
    let rule = RateLimitRule::upstream(Uuid::from_u128(0x1), &config);
    let key = rule.key(&subject());
    let effective = EffectiveLimit::of(&config);

    limiter
        .check(key.as_str(), &effective)
        .allowed()
        .expect("first");
    assert!(limiter.check(key.as_str(), &effective).is_rejected());
    clock.advance(60_000);
    limiter
        .check(key.as_str(), &effective)
        .allowed()
        .expect("the window has passed entirely");
}
