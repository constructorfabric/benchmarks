#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Unit tests of the in-memory rate limiter, on a clock the tests advance by
//! hand so no test sleeps.

use std::sync::Arc;
use std::time::Duration;

use http::HeaderMap;
use serde_json::json;
use uuid::Uuid;

use super::{
    LimitResource, LimitScope, ManualClock, RATE_LIMIT_KEY_PREFIX, RateLimitClock,
    RateLimitDecision, RateLimitIdentity, RateLimiter, rate_limit_key,
};
use crate::domain::merger::EffectiveRateLimit;
use crate::domain::model::{RateLimit, RateLimitStrategy, RateLimitWindow};
use http::HeaderValue;

const UNIX_START: u64 = 1_769_000_000;

/// A `rate_limit` block with the fields the test names, everything else at its
/// schema default.
fn limit(
    algorithm: &str,
    rate: u32,
    window: &str,
    capacity: Option<u32>,
    scope: &str,
    cost: u32,
) -> RateLimit {
    serde_json::from_value(json!({
        "sharing": "private",
        "algorithm": algorithm,
        "sustained": { "rate": rate, "window": window },
        "burst": capacity.map(|value| json!({ "capacity": value })),
        "scope": scope,
        "cost": cost,
    }))
    .expect("a valid rate_limit block")
}

/// A token-bucket policy of `rate` per second and a burst of `capacity`.
fn token_bucket(rate: u32, capacity: u32) -> EffectiveRateLimit {
    EffectiveRateLimit::of(&limit(
        "token_bucket",
        rate,
        "second",
        Some(capacity),
        "tenant",
        1,
    ))
}

/// A sliding-window policy of `rate` per `window`.
fn sliding_window(rate: u32, window: &str) -> EffectiveRateLimit {
    EffectiveRateLimit::of(&limit("sliding_window", rate, window, None, "tenant", 1))
}

/// A limiter on a clock the test advances by hand.
fn limiter() -> (Arc<ManualClock>, super::RateLimiter) {
    let clock = Arc::new(ManualClock::start_at(UNIX_START));
    let limiter = super::RateLimiter::new(Arc::clone(&clock) as Arc<dyn RateLimitClock>);
    (clock, limiter)
}

fn identity() -> (Uuid, Uuid, RateLimitIdentity) {
    let tenant = Uuid::new_v4();
    let upstream = Uuid::new_v4();
    (tenant, upstream, RateLimitIdentity::new(tenant, upstream))
}

/// Checks `policy` for an upstream of `identity`.
fn check(
    limiter: &RateLimiter,
    policy: &EffectiveRateLimit,
    identity: &RateLimitIdentity,
) -> RateLimitDecision {
    limiter.check(
        policy,
        &LimitResource::upstream(identity.upstream_id),
        identity,
    )
}

/// The value of a header the limiter emitted, as text.
fn header<'h>(headers: &'h HeaderMap, name: &str) -> &'h str {
    headers
        .get(name)
        .expect("the limiter emitted the header")
        .to_str()
        .expect("an ascii header value")
}

#[test]
fn the_key_spells_out_every_adr_0003_segment() {
    let tenant = Uuid::new_v4();
    let upstream = Uuid::new_v4();
    let identity = RateLimitIdentity::new(tenant, upstream);

    let key = rate_limit_key(
        &LimitResource::upstream(upstream),
        LimitScope::Tenant,
        &identity,
        RateLimitWindow::Minute,
    );
    assert_eq!(
        key,
        format!("oagw:ratelimit:upstream:{upstream}:tenant:{tenant}:minute")
    );
    assert!(key.starts_with(RATE_LIMIT_KEY_PREFIX));

    let route = Uuid::new_v4();
    let key = rate_limit_key(
        &LimitResource::route(route),
        LimitScope::Subject,
        &identity.with_subject(Uuid::new_v4()),
        RateLimitWindow::Day,
    );
    assert!(
        key.contains(":route:"),
        "the resource kind is in the key: {key}"
    );
    assert!(key.ends_with(":day"));
}

#[test]
fn every_scope_id_is_rooted_in_the_tenant() {
    let tenant = Uuid::new_v4();
    let identity = RateLimitIdentity::new(tenant, Uuid::new_v4())
        .with_route(Uuid::new_v4())
        .with_subject(Uuid::new_v4())
        .with_client_ip("203.0.113.7".parse().expect("an ip address"));

    for scope in [
        LimitScope::Global,
        LimitScope::Tenant,
        LimitScope::Subject,
        LimitScope::Ip,
        LimitScope::Route,
        LimitScope::Upstream,
    ] {
        let key = rate_limit_key(
            &LimitResource::upstream(identity.upstream_id),
            scope,
            &identity,
            RateLimitWindow::Second,
        );
        assert!(
            key.contains(&tenant.to_string()),
            "{scope:?} lost the tenant: {key}"
        );
    }
}

#[test]
fn a_subject_scope_keys_by_caller_and_an_ip_scope_by_address() {
    let tenant = Uuid::new_v4();
    let identity = RateLimitIdentity::new(tenant, Uuid::new_v4());
    let caller_a = Uuid::new_v4();
    let caller_b = Uuid::new_v4();

    let of_caller_a = rate_limit_key(
        &LimitResource::upstream(identity.upstream_id),
        LimitScope::Subject,
        &identity.clone().with_subject(caller_a),
        RateLimitWindow::Second,
    );
    let of_caller_b = rate_limit_key(
        &LimitResource::upstream(identity.upstream_id),
        LimitScope::Subject,
        &identity.clone().with_subject(caller_b),
        RateLimitWindow::Second,
    );
    assert_ne!(
        of_caller_a, of_caller_b,
        "each caller counts on its own key"
    );

    let of_ip = rate_limit_key(
        &LimitResource::upstream(identity.upstream_id),
        LimitScope::Ip,
        &identity.with_client_ip("203.0.113.7".parse().expect("an ip address")),
        RateLimitWindow::Second,
    );
    assert!(
        of_ip.contains("203.0.113.7"),
        "the ip scopes the key: {of_ip}"
    );
}

#[test]
fn a_fresh_bucket_admits_a_burst_of_capacity_then_rejects_with_429() {
    let (_clock, limiter) = limiter();
    // 10 requests per second, burst capacity 5.
    let policy = token_bucket(10, 5);
    let (_tenant, _upstream, identity) = identity();

    for remaining in (0u32..5).rev() {
        let decision = check(&limiter, &policy, &identity);
        assert!(
            decision.allowed,
            "request {} of the burst is admitted",
            5 - remaining
        );
        assert_eq!(decision.remaining, u64::from(remaining));
    }

    let rejected = check(&limiter, &policy, &identity);
    assert!(!rejected.allowed, "the 6th request is over the burst");
    assert_eq!(rejected.error().http_status(), 429);
    assert_eq!(rejected.remaining, 0);
    assert!(
        rejected.retry_after_secs >= 1,
        "a rejection always names a retry hint"
    );
}

#[test]
fn a_rejected_request_consumes_no_token() {
    let (clock, limiter) = limiter();
    let policy = token_bucket(10, 5);
    let (_tenant, _upstream, identity) = identity();

    for _ in 0..5 {
        assert!(check(&limiter, &policy, &identity).allowed);
    }
    assert!(
        !check(&limiter, &policy, &identity).allowed,
        "the burst is spent"
    );
    assert!(
        !check(&limiter, &policy, &identity).allowed,
        "retrying cannot drain the bucket further"
    );

    // One second refills the 10/s bucket past the capacity it clamps at, so the
    // next request is admitted and the bucket holds no more than it did.
    clock.advance_secs(1);
    let decision = check(&limiter, &policy, &identity);
    assert!(decision.allowed);
    assert_eq!(
        decision.remaining, 4,
        "the bucket never exceeds its capacity"
    );
}

#[test]
fn tokens_refill_at_the_sustained_rate() {
    let (clock, limiter) = limiter();
    // 2 per second, burst capacity 2.
    let policy = token_bucket(2, 2);
    let (_tenant, _upstream, identity) = identity();

    assert!(check(&limiter, &policy, &identity).allowed);
    assert!(check(&limiter, &policy, &identity).allowed);
    assert!(!check(&limiter, &policy, &identity).allowed);

    // Half a second refills exactly one token.
    clock.advance(Duration::from_millis(500));
    let decision = check(&limiter, &policy, &identity);
    assert!(decision.allowed, "one token refilled");
    assert_eq!(decision.remaining, 0);
    assert!(
        !check(&limiter, &policy, &identity).allowed,
        "no token is left"
    );
}

#[test]
fn the_cost_charges_more_than_one_token() {
    let (_clock, limiter) = limiter();
    let policy = EffectiveRateLimit::of(&limit("token_bucket", 10, "second", Some(5), "tenant", 3));
    let (_tenant, _upstream, identity) = identity();

    let first = check(&limiter, &policy, &identity);
    assert!(first.allowed);
    assert_eq!(first.remaining, 2);

    let second = check(&limiter, &policy, &identity);
    assert!(!second.allowed, "2 tokens cannot pay a cost of 3");
    assert_eq!(second.remaining, 2, "a rejection is not charged");
}

#[test]
fn retry_after_reports_when_the_next_token_refills() {
    let (_clock, limiter) = limiter();
    // 1 per second: a spent bucket needs a whole second for its next token.
    let policy = token_bucket(1, 1);
    let (_tenant, _upstream, identity) = identity();

    assert!(check(&limiter, &policy, &identity).allowed);
    let rejected = check(&limiter, &policy, &identity);
    assert!(!rejected.allowed);
    assert_eq!(rejected.retry_after_secs, 1, "rounded up, never zero");
}

#[test]
fn reset_at_names_the_instant_the_bucket_is_full_again() {
    let (clock, limiter) = limiter();
    let policy = token_bucket(10, 5);
    let (_tenant, _upstream, identity) = identity();

    let decision = check(&limiter, &policy, &identity);
    // The one token the request spent refills in a tenth of a second, rounded up
    // to the second the header reports.
    assert_eq!(decision.reset_at, UNIX_START + 1);

    clock.advance_secs(2);
    let decision = check(&limiter, &policy, &identity);
    assert_eq!(
        decision.reset_at,
        UNIX_START + 3,
        "refilled from the instant of the check"
    );
}

#[test]
fn a_sliding_window_counts_requests_per_window() {
    let (clock, limiter) = limiter();
    let policy = sliding_window(3, "minute");
    let (_tenant, _upstream, identity) = identity();

    for _ in 0..3 {
        let decision = check(&limiter, &policy, &identity);
        assert!(decision.allowed);
        assert_eq!(decision.limit, 3, "the window enforces its rate");
    }
    let rejected = check(&limiter, &policy, &identity);
    assert!(!rejected.allowed);
    assert_eq!(rejected.remaining, 0);

    clock.advance_secs(60);
    let decision = check(&limiter, &policy, &identity);
    assert!(decision.allowed, "the window rolled over");
    assert_eq!(decision.remaining, 2);
}

#[test]
fn a_sliding_window_frees_room_when_its_oldest_hit_ages_out() {
    let (clock, limiter) = limiter();
    let policy = sliding_window(2, "second");
    let (_tenant, _upstream, identity) = identity();

    assert!(check(&limiter, &policy, &identity).allowed);
    assert!(check(&limiter, &policy, &identity).allowed);
    clock.advance(Duration::from_millis(500));
    assert!(
        !check(&limiter, &policy, &identity).allowed,
        "still inside the window"
    );

    clock.advance(Duration::from_millis(600));
    assert!(
        check(&limiter, &policy, &identity).allowed,
        "the first hit aged out"
    );
}

#[test]
fn counters_are_separate_per_tenant() {
    let (_clock, limiter) = limiter();
    let policy = token_bucket(10, 1);
    let upstream = Uuid::new_v4();

    let tenant_a = Uuid::new_v4();
    let tenant_b = Uuid::new_v4();
    let first = RateLimitIdentity::new(tenant_a, upstream);
    let second = RateLimitIdentity::new(tenant_b, upstream);

    assert!(check(&limiter, &policy, &first).allowed);
    assert!(!check(&limiter, &policy, &first).allowed);
    // The other tenant's bucket is untouched.
    let decision = check(&limiter, &policy, &second);
    assert!(decision.allowed);
    assert_ne!(decision.key, "");
    assert!(decision.key.contains(&tenant_b.to_string()));
}

#[test]
fn counters_are_separate_per_scope_identity() {
    let (_clock, limiter) = limiter();
    let policy = EffectiveRateLimit::of(&limit("token_bucket", 10, "second", Some(1), "user", 1));
    let tenant = Uuid::new_v4();
    let upstream = Uuid::new_v4();

    let caller_a = RateLimitIdentity::new(tenant, upstream).with_subject(Uuid::new_v4());
    let caller_b = RateLimitIdentity::new(tenant, upstream).with_subject(Uuid::new_v4());

    assert!(check(&limiter, &policy, &caller_a).allowed);
    assert!(!check(&limiter, &policy, &caller_a).allowed);
    assert!(
        check(&limiter, &policy, &caller_b).allowed,
        "the other caller is not capped"
    );
}

#[test]
fn an_omitted_scope_identity_still_keys_per_tenant() {
    let (_clock, limiter) = limiter();
    let policy = EffectiveRateLimit::of(&limit("token_bucket", 10, "second", Some(1), "global", 1));
    let tenant = Uuid::new_v4();
    let identity = RateLimitIdentity::new(tenant, Uuid::new_v4());

    let decision = check(&limiter, &policy, &identity);
    assert!(decision.allowed);
    assert!(
        decision.key.contains(&tenant.to_string()),
        "a global counter stays per tenant"
    );
}

#[test]
fn the_headers_carry_the_rate_limit_state() {
    let (_clock, limiter) = limiter();
    let policy = token_bucket(10, 2);
    let (_tenant, _upstream, identity) = identity();

    let allowed = check(&limiter, &policy, &identity);
    let headers: HeaderMap = allowed.headers(&policy);
    assert_eq!(header(&headers, "x-ratelimit-limit"), "2");
    assert_eq!(header(&headers, "x-ratelimit-remaining"), "1");
    assert_eq!(
        header(&headers, "x-ratelimit-reset"),
        (UNIX_START + 1).to_string()
    );
    assert!(
        headers.get("retry-after").is_none(),
        "an admission needs no retry hint"
    );

    check(&limiter, &policy, &identity);
    let rejected = check(&limiter, &policy, &identity);
    assert_eq!(header(&rejected.headers(&policy), "retry-after"), "1");
}

#[test]
fn turning_the_headers_off_keeps_the_retry_hint() {
    let (_clock, limiter) = limiter();
    let mut policy = token_bucket(10, 1);
    policy.response_headers = false;
    let (_tenant, _upstream, identity) = identity();

    let allowed = check(&limiter, &policy, &identity);
    let headers = allowed.headers(&policy);
    assert!(
        headers.get("x-ratelimit-limit").is_none(),
        "the policy opted out"
    );

    let rejected = check(&limiter, &policy, &identity);
    let headers = rejected.headers(&policy);
    assert!(headers.get("x-ratelimit-limit").is_none());
    assert!(
        headers.get("retry-after").is_some(),
        "Retry-After is never dropped"
    );
}

#[test]
fn every_header_value_is_well_formed() {
    let (_clock, limiter) = limiter();
    let policy = token_bucket(10, 1);
    let (_tenant, _upstream, identity) = identity();

    check(&limiter, &policy, &identity);
    let headers = check(&limiter, &policy, &identity).headers(&policy);
    for name in ["x-ratelimit-limit", "x-ratelimit-reset", "retry-after"] {
        let value = header(&headers, name);
        assert!(HeaderValue::from_str(value).is_ok(), "{name}: {value}");
    }
}

#[test]
fn a_rejection_is_the_design_rate_limit_error() {
    let (_clock, limiter) = limiter();
    let policy = token_bucket(10, 1);
    let (_tenant, _upstream, identity) = identity();

    check(&limiter, &policy, &identity);
    let rejected = check(&limiter, &policy, &identity);
    let error = rejected.error();
    assert_eq!(error.http_status(), 429);
    assert!(
        error.gts_id().contains("cf.oagw.rate_limit.exceeded"),
        "{}",
        error.gts_id()
    );
}

#[test]
fn the_strategy_travels_with_the_decision() {
    let (_clock, limiter) = limiter();
    let mut policy = token_bucket(10, 1);
    policy.strategy = RateLimitStrategy::Queue;
    let (_tenant, _upstream, identity) = identity();

    assert_eq!(
        check(&limiter, &policy, &identity).strategy,
        RateLimitStrategy::Queue
    );
}

#[test]
fn forget_resource_drops_only_that_resources_counters() {
    let (_clock, limiter) = limiter();
    let policy = token_bucket(10, 1);
    let tenant = Uuid::new_v4();
    let upstream = Uuid::new_v4();
    let other = Uuid::new_v4();
    let identity = RateLimitIdentity::new(tenant, upstream);
    let other_identity = RateLimitIdentity::new(tenant, other);

    assert!(check(&limiter, &policy, &identity).allowed);
    assert!(!check(&limiter, &policy, &identity).allowed);
    assert!(check(&limiter, &policy, &other_identity).allowed);
    assert_eq!(limiter.len(), 2);

    limiter.forget_resource(&LimitResource::upstream(upstream));
    assert_eq!(limiter.len(), 1, "the other resource keeps its counter");
    assert!(
        check(&limiter, &policy, &identity).allowed,
        "the counter was dropped"
    );
    assert!(
        !check(&limiter, &policy, &other_identity).allowed,
        "it was not reset"
    );
}

#[test]
fn a_fresh_limiter_holds_no_counter() {
    let (_clock, limiter) = limiter();
    assert!(limiter.is_empty());
    assert_eq!(limiter.len(), 0);

    let policy = token_bucket(10, 1);
    let (_tenant, _upstream, identity) = identity();
    check(&limiter, &policy, &identity);
    assert_eq!(limiter.len(), 1);
}

#[test]
fn the_manual_clock_advances_unix_time_with_itself() {
    let clock = ManualClock::start_at(UNIX_START);
    assert_eq!(clock.unix_now(), UNIX_START);
    clock.advance_secs(90);
    assert_eq!(clock.unix_now(), UNIX_START + 90);
    clock.advance(Duration::from_millis(500));
    assert_eq!(
        clock.unix_now(),
        UNIX_START + 90,
        "sub-second time is not unix seconds"
    );
}
