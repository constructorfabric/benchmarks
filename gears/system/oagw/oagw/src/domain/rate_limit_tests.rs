//! Tests for [`crate::domain::rate_limit`].

use std::time::{Duration, Instant};

use axum::http::HeaderValue;
use uuid::Uuid;

use super::{
    DEGRADED_HEADER, DEGRADED_HEADER_VALUE, EffectiveRateLimit, MAX_BUCKETS, QUEUE_MAX_WAIT,
    RATE_LIMIT_EXCEEDED_TYPE, RATE_LIMIT_HEADER, RATE_LIMIT_REMAINING_HEADER,
    RATE_LIMIT_RESET_HEADER, RETRY_AFTER_HEADER, RateLimitScopeValues, RateLimiterRegistry,
    SlidingWindow, TokenBucket, resolve_effective_rate_limit, window_duration,
};
use crate::domain::model::{
    BurstConfig, RateLimitAlgorithm, RateLimitConfig, RateLimitScope, RateLimitStrategy,
    RateLimitWindow, SharingMode, SustainedRateConfig,
};

const UPSTREAM: Uuid = Uuid::from_u128(0x77);

fn start() -> Instant {
    // A fixed reference point; every test works with relative offsets.
    Instant::now()
}

fn sustained(rate: u64, window: RateLimitWindow) -> SustainedRateConfig {
    SustainedRateConfig { rate, window }
}

fn config(
    rate: u64,
    window: RateLimitWindow,
    capacity: Option<u64>,
    sharing: SharingMode,
) -> RateLimitConfig {
    RateLimitConfig {
        sharing,
        algorithm: RateLimitAlgorithm::TokenBucket,
        sustained: sustained(rate, window),
        burst: capacity.map(|value| BurstConfig {
            capacity: Some(value),
        }),
        scope: RateLimitScope::Tenant,
        strategy: RateLimitStrategy::Reject,
        response_headers: true,
        cost: 1,
    }
}

fn effective(rate: u64, window: RateLimitWindow, capacity: Option<u64>) -> EffectiveRateLimit {
    EffectiveRateLimit::from_config(&config(rate, window, capacity, SharingMode::Private))
}

fn values(
    tenant: &str,
    subject: Option<&str>,
    peer: Option<&str>,
    route: Option<&str>,
) -> RateLimitScopeValues {
    RateLimitScopeValues {
        tenant_id: Some(tenant.to_owned()),
        subject_id: subject.map(str::to_owned),
        peer_ip: peer.map(str::to_owned),
        route_id: route.map(str::to_owned),
    }
}

fn header_value(headers: &[(axum::http::HeaderName, HeaderValue)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(candidate, _)| candidate.as_str() == name)
        .map(|(_, value)| value.to_str().unwrap_or_default().to_owned())
}

// ---------------------------------------------------------------------------
// Window mapping
// ---------------------------------------------------------------------------

#[test]
fn window_durations_follow_the_schema() {
    assert_eq!(
        window_duration(RateLimitWindow::Second),
        Duration::from_secs(1)
    );
    assert_eq!(
        window_duration(RateLimitWindow::Minute),
        Duration::from_secs(60)
    );
    assert_eq!(
        window_duration(RateLimitWindow::Hour),
        Duration::from_secs(3_600)
    );
    assert_eq!(
        window_duration(RateLimitWindow::Day),
        Duration::from_secs(86_400)
    );
}

#[test]
fn effective_config_projects_the_schema_fields() {
    let mut config = config(600, RateLimitWindow::Minute, None, SharingMode::Private);
    config.strategy = RateLimitStrategy::Queue;
    config.cost = 3;
    config.scope = RateLimitScope::User;
    config.response_headers = false;
    let effective = EffectiveRateLimit::from_config(&config);
    assert_eq!(effective.sustained_rate, 600);
    assert_eq!(effective.window, RateLimitWindow::Minute);
    assert_eq!(
        effective.capacity, 600,
        "capacity defaults to the sustained rate"
    );
    assert_eq!(effective.strategy, RateLimitStrategy::Queue);
    assert_eq!(effective.cost, 3);
    assert_eq!(effective.scope, RateLimitScope::User);
    assert!(!effective.response_headers);
    assert!(
        (effective.refill_rate() - 10.0).abs() < 1e-9,
        "600/min is 10/s"
    );
}

#[test]
fn effective_config_honours_an_explicit_capacity_and_a_zero_cost() {
    let mut config = config(
        100,
        RateLimitWindow::Second,
        Some(1_000),
        SharingMode::Private,
    );
    config.cost = 0;
    let effective = EffectiveRateLimit::from_config(&config);
    assert_eq!(effective.capacity, 1_000);
    assert_eq!(effective.cost, 1, "a zero cost is clamped to one token");
}

// ---------------------------------------------------------------------------
// Inheritance
// ---------------------------------------------------------------------------

#[test]
fn no_entry_yields_none() {
    let chain: Vec<&RateLimitConfig> = Vec::new();
    assert!(resolve_effective_rate_limit(&chain).is_none());
}

#[test]
fn a_single_entry_is_used_verbatim() {
    let outer = config(50, RateLimitWindow::Second, Some(80), SharingMode::Private);
    let effective = resolve_effective_rate_limit(&[&outer]).expect("effective");
    assert_eq!(effective.sustained_rate, 50);
    assert_eq!(effective.capacity, 80);
}

#[test]
fn private_parent_hides_the_limit_from_the_child() {
    let parent = config(1_000, RateLimitWindow::Second, None, SharingMode::Private);
    let child = config(10, RateLimitWindow::Minute, None, SharingMode::Inherit);
    let effective = resolve_effective_rate_limit(&[&parent, &child]).expect("effective");
    assert_eq!(effective.sustained_rate, 10);
    assert_eq!(effective.window, RateLimitWindow::Minute);
}

#[test]
fn inherit_with_a_child_limit_takes_the_minimum() {
    let parent = config(100, RateLimitWindow::Second, None, SharingMode::Inherit);
    let child = config(1_000, RateLimitWindow::Second, None, SharingMode::Private);
    let effective = resolve_effective_rate_limit(&[&parent, &child]).expect("effective");
    assert_eq!(effective.sustained_rate, 100);
}

#[test]
fn inherit_compares_rates_across_different_windows() {
    // 100/minute (1.67/s) is more restrictive than 10/second.
    let parent = config(100, RateLimitWindow::Minute, None, SharingMode::Inherit);
    let child = config(10, RateLimitWindow::Second, None, SharingMode::Private);
    let effective = resolve_effective_rate_limit(&[&parent, &child]).expect("effective");
    assert_eq!(effective.sustained_rate, 100);
    assert_eq!(effective.window, RateLimitWindow::Minute);
}

#[test]
fn enforce_always_merges_the_limits() {
    let parent = config(500, RateLimitWindow::Minute, None, SharingMode::Enforce);
    let child = config(100, RateLimitWindow::Minute, None, SharingMode::Private);
    let effective = resolve_effective_rate_limit(&[&parent, &child]).expect("effective");
    assert_eq!(effective.sustained_rate, 100);

    let child = config(1_000, RateLimitWindow::Minute, None, SharingMode::Private);
    let effective = resolve_effective_rate_limit(&[&parent, &child]).expect("effective");
    assert_eq!(effective.sustained_rate, 500);
}

#[test]
fn three_level_chain_uses_the_most_restrictive_limit() {
    let root = config(1_000, RateLimitWindow::Minute, None, SharingMode::Inherit);
    let middle = config(50, RateLimitWindow::Second, None, SharingMode::Inherit);
    let leaf = config(10, RateLimitWindow::Second, None, SharingMode::Private);
    let effective = resolve_effective_rate_limit(&[&root, &middle, &leaf]).expect("effective");
    assert_eq!(effective.sustained_rate, 10);
}

#[test]
fn inheritance_keeps_the_winning_capacity() {
    let parent = config(
        100,
        RateLimitWindow::Second,
        Some(200),
        SharingMode::Inherit,
    );
    let child = config(
        10,
        RateLimitWindow::Second,
        Some(1_000),
        SharingMode::Private,
    );
    let effective = resolve_effective_rate_limit(&[&parent, &child]).expect("effective");
    assert_eq!(effective.sustained_rate, 10);
    assert_eq!(
        effective.capacity, 1_000,
        "the winning entry shapes the bucket"
    );
}

// ---------------------------------------------------------------------------
// Token bucket
// ---------------------------------------------------------------------------

#[test]
fn bucket_starts_full_and_refills_at_the_configured_rate() {
    let now = start();
    let mut bucket = TokenBucket::new(10.0, 2.0, now);
    assert_eq!(bucket.tokens, 10.0);
    bucket.refill(now + Duration::from_secs(1));
    assert_eq!(
        bucket.tokens, 10.0,
        "a full bucket cannot exceed its capacity"
    );
    assert_eq!(bucket.last_refill, now + Duration::from_secs(1));
}

#[test]
fn bucket_refill_follows_the_reference_formula() {
    let now = start();
    let mut bucket = TokenBucket::new(10.0, 2.0, now);
    bucket.tokens = 0.0;
    bucket.refill(now + Duration::from_millis(1_500));
    assert!(
        (bucket.tokens - 3.0).abs() < 1e-9,
        "1.5s at 2/s is 3 tokens"
    );
}

#[test]
fn try_acquire_debits_and_refuses_when_empty() {
    let now = start();
    let mut bucket = TokenBucket::new(2.0, 0.0, now);
    assert!(bucket.try_acquire(1.0, now));
    assert!(bucket.try_acquire(1.0, now));
    assert!(!bucket.try_acquire(1.0, now), "capacity exhausted");
    assert_eq!(bucket.tokens, 0.0);
}

#[test]
fn try_acquire_refills_before_debiting() {
    let now = start();
    let mut bucket = TokenBucket::new(1.0, 1.0, now);
    assert!(bucket.try_acquire(1.0, now));
    assert!(!bucket.try_acquire(1.0, now + Duration::from_millis(500)));
    assert!(
        bucket.try_acquire(1.0, now + Duration::from_secs(1)),
        "1 token refilled"
    );
    assert!((bucket.tokens_at(now + Duration::from_secs(1)) - 0.0).abs() < 1e-9);
}

#[test]
fn try_acquire_leaves_the_bucket_untouched_on_rejection() {
    let now = start();
    let mut bucket = TokenBucket::new(1.0, 0.0, now);
    assert!(bucket.try_acquire(1.0, now));
    assert!(!bucket.try_acquire(1.0, now));
    assert_eq!(bucket.tokens, 0.0);
}

#[test]
fn burst_capacity_absorbs_a_burst_then_throttles() {
    let now = start();
    // 10 tokens burst, 1 token per second.
    let mut bucket = TokenBucket::new(10.0, 1.0, now);
    for _ in 0..10 {
        assert!(bucket.try_acquire(1.0, now), "the burst must be absorbed");
    }
    assert!(!bucket.try_acquire(1.0, now));
    assert!(
        bucket.try_acquire(1.0, now + Duration::from_secs(1)),
        "one token refilled"
    );
}

#[test]
fn time_to_tokens_is_the_deficit_over_the_rate() {
    let now = start();
    let mut bucket = TokenBucket::new(10.0, 2.0, now);
    bucket.tokens = 0.0;
    assert_eq!(bucket.time_to_tokens(1.0, now), Duration::from_millis(500));
    assert_eq!(bucket.time_to_tokens(10.0, now), Duration::from_secs(5));
}

#[test]
fn time_to_tokens_is_zero_when_the_target_is_already_available() {
    let now = start();
    let bucket = TokenBucket::new(10.0, 2.0, now);
    assert_eq!(bucket.time_to_tokens(5.0, now), Duration::ZERO);
}

#[test]
fn a_stalled_bucket_never_replenishes() {
    let now = start();
    let mut bucket = TokenBucket::new(1.0, 0.0, now);
    bucket.tokens = 0.0;
    assert_eq!(bucket.time_to_tokens(1.0, now), Duration::MAX);
}

// ---------------------------------------------------------------------------
// Sliding window
// ---------------------------------------------------------------------------

#[test]
fn sliding_window_counts_hits_inside_the_window() {
    let now = start();
    let mut window = SlidingWindow::new(3.0, Duration::from_secs(10));
    assert!(window.try_acquire(1.0, now));
    assert!(window.try_acquire(1.0, now));
    assert!(window.try_acquire(1.0, now));
    assert!(!window.try_acquire(1.0, now));
    assert_eq!(window.remaining(now), 0.0);
}

#[test]
fn sliding_window_frees_capacity_as_hits_expire() {
    let now = start();
    let mut window = SlidingWindow::new(3.0, Duration::from_secs(10));
    for offset in 0..3 {
        assert!(window.try_acquire(1.0, now + Duration::from_secs(offset)));
    }
    assert_eq!(window.remaining(now + Duration::from_secs(2)), 0.0);
    assert_eq!(
        window.remaining(now + Duration::from_secs(10)),
        1.0,
        "the first hit expired"
    );
}

#[test]
fn sliding_window_time_to_tokens_reports_the_expiry() {
    let now = start();
    let mut window = SlidingWindow::new(2.0, Duration::from_secs(10));
    assert!(window.try_acquire(2.0, now));
    assert_eq!(
        window.time_to_tokens(1.0, now + Duration::from_secs(1)),
        Duration::from_secs(9)
    );
}

#[test]
fn sliding_window_time_to_reset_waits_for_the_oldest_hit() {
    let now = start();
    let mut window = SlidingWindow::new(3.0, Duration::from_secs(10));
    assert!(window.try_acquire(1.0, now));
    assert!(window.try_acquire(1.0, now + Duration::from_secs(4)));
    assert_eq!(
        window.time_to_reset(now + Duration::from_secs(4)),
        Duration::from_secs(6)
    );
    assert_eq!(
        window.time_to_reset(now + Duration::from_secs(40)),
        Duration::ZERO
    );
}

// ---------------------------------------------------------------------------
// Registry and strategies
// ---------------------------------------------------------------------------

#[test]
fn scope_identifiers_fall_back_to_the_tenant() {
    let scoped = values(
        "tenant-a",
        Some("subject-a"),
        Some("10.0.0.1"),
        Some("route-a"),
    );
    assert_eq!(scoped.identifier(RateLimitScope::Tenant), "tenant-a");
    assert_eq!(scoped.identifier(RateLimitScope::User), "subject-a");
    assert_eq!(scoped.identifier(RateLimitScope::Ip), "10.0.0.1");
    assert_eq!(scoped.identifier(RateLimitScope::Route), "route-a");
    assert_eq!(scoped.identifier(RateLimitScope::Global), "unscoped");
}

#[test]
fn missing_scope_values_never_widen_the_limit() {
    let scoped = values("tenant-a", None, None, None);
    assert_eq!(scoped.identifier(RateLimitScope::User), "tenant-a");
    assert_eq!(scoped.identifier(RateLimitScope::Ip), "tenant-a");
    assert_eq!(scoped.identifier(RateLimitScope::Route), "tenant-a");
}

#[test]
fn registry_keys_are_per_upstream_and_per_scope() {
    let registry = RateLimiterRegistry::new();
    let effective = effective(10, RateLimitWindow::Second, Some(10));
    let left = registry.scope_key(
        UPSTREAM,
        RateLimitScope::Tenant,
        &values("t1", None, None, None),
    );
    let right = registry.scope_key(
        UPSTREAM,
        RateLimitScope::User,
        &values("t1", Some("s1"), None, None),
    );
    assert_ne!(left, right);
    let other = registry.scope_key(
        Uuid::from_u128(0x78),
        RateLimitScope::Tenant,
        &values("t1", None, None, None),
    );
    assert_ne!(left, other);

    let now = start();
    let epoch = 1_000;
    let first = registry.check(
        UPSTREAM,
        &effective,
        &values("t1", None, None, None),
        now,
        epoch,
    );
    let second = registry.check(
        UPSTREAM,
        &effective,
        &values("t1", None, None, None),
        now,
        epoch,
    );
    assert!(first.is_allowed());
    assert!(second.is_allowed());
    assert_eq!(registry.len(), 1);
    assert!(!registry.is_empty());
}

#[test]
fn registry_tracks_one_counter_per_tenant() {
    let registry = RateLimiterRegistry::new();
    let effective = effective(1, RateLimitWindow::Second, Some(1));
    let now = start();
    let epoch = 1_000;
    let first = registry.check(
        UPSTREAM,
        &effective,
        &values("t1", None, None, None),
        now,
        epoch,
    );
    let second = registry.check(
        UPSTREAM,
        &effective,
        &values("t1", None, None, None),
        now,
        epoch,
    );
    let other = registry.check(
        UPSTREAM,
        &effective,
        &values("t2", None, None, None),
        now,
        epoch,
    );
    assert!(first.is_allowed());
    assert!(second.is_limited());
    assert!(other.is_allowed());
    assert_eq!(registry.len(), 2);
}

#[test]
fn reject_strategy_emits_the_adr_headers_and_retry_after() {
    let registry = RateLimiterRegistry::new();
    let effective = effective(1, RateLimitWindow::Second, Some(1));
    let now = start();
    let epoch = 1_000;
    let granted = registry.check(
        UPSTREAM,
        &effective,
        &values("t1", None, None, None),
        now,
        epoch,
    );
    assert!(granted.is_allowed());
    let headers = granted.headers();
    assert_eq!(
        header_value(&headers, RATE_LIMIT_HEADER).as_deref(),
        Some("1")
    );
    assert_eq!(
        header_value(&headers, RATE_LIMIT_REMAINING_HEADER).as_deref(),
        Some("0")
    );
    // The single token is spent at epoch 1000 and refills one second later.
    assert_eq!(
        header_value(&headers, RATE_LIMIT_RESET_HEADER).as_deref(),
        Some("1001")
    );
    assert_eq!(header_value(&headers, RETRY_AFTER_HEADER), None);

    let rejected = registry.check(
        UPSTREAM,
        &effective,
        &values("t1", None, None, None),
        now,
        epoch,
    );
    assert!(rejected.is_limited());
    assert_eq!(rejected.retry_after_seconds, Some(1));
    let headers = rejected.headers();
    assert_eq!(
        header_value(&headers, RETRY_AFTER_HEADER).as_deref(),
        Some("1")
    );

    let error = rejected.into_error();
    assert_eq!(error.status(), axum::http::StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(error.problem_body().r#type, RATE_LIMIT_EXCEEDED_TYPE);
    assert_eq!(error.problem_body().context.retry_after_seconds, Some(1));
}

#[test]
fn headers_are_suppressed_when_the_configuration_disables_them() {
    let mut config = config(1, RateLimitWindow::Second, Some(1), SharingMode::Private);
    config.response_headers = false;
    let effective = EffectiveRateLimit::from_config(&config);
    let registry = RateLimiterRegistry::new();
    let decision = registry.check(
        UPSTREAM,
        &effective,
        &values("t1", None, None, None),
        start(),
        1_000,
    );
    assert!(decision.is_allowed());
    assert!(decision.headers().is_empty());
}

#[test]
fn queue_strategy_grants_a_short_wait_within_the_bound() {
    let registry = RateLimiterRegistry::new();
    let mut config = config(2, RateLimitWindow::Second, Some(1), SharingMode::Private);
    config.strategy = RateLimitStrategy::Queue;
    let effective = EffectiveRateLimit::from_config(&config);
    let now = start();
    let first = registry.check(
        UPSTREAM,
        &effective,
        &values("t1", None, None, None),
        now,
        1_000,
    );
    assert!(first.is_allowed());
    assert_eq!(first.queue_wait, None);

    // 500 ms of wait is within QUEUE_MAX_WAIT, so the request is queued.
    let second = registry.check(
        UPSTREAM,
        &effective,
        &values("t1", None, None, None),
        now,
        1_000,
    );
    assert!(second.is_allowed());
    assert_eq!(second.queue_wait, Some(Duration::from_millis(500)));
    assert!(second.queue_wait.unwrap() <= QUEUE_MAX_WAIT);
}

#[test]
fn queue_strategy_rejects_a_wait_beyond_the_bound() {
    let registry = RateLimiterRegistry::new();
    // One token per minute: once the bucket is spent the projected wait for
    // the next token is a full minute, far beyond QUEUE_MAX_WAIT.
    let mut config = config(1, RateLimitWindow::Minute, Some(1), SharingMode::Private);
    config.strategy = RateLimitStrategy::Queue;
    let effective = EffectiveRateLimit::from_config(&config);
    let now = start();
    let epoch = 1_000;
    let granted = registry.check(
        UPSTREAM,
        &effective,
        &values("t1", None, None, None),
        now,
        epoch,
    );
    assert!(granted.is_allowed());
    let rejected = registry.check(
        UPSTREAM,
        &effective,
        &values("t1", None, None, None),
        now,
        epoch,
    );
    assert!(
        rejected.is_limited(),
        "a minute of waiting is not queueable"
    );
    assert!(rejected.queue_wait.is_none());
    let retry_after = rejected.retry_after_seconds.expect("retry after");
    assert!(retry_after > 1, "retry-after must exceed the queue bound");
}

#[test]
fn a_queue_strategy_waits_where_a_reject_strategy_refuses() {
    // The same bucket (rate 2/s, capacity 1) under both strategies: the queue
    // strategy reserves a token and hands back a bounded wait, the reject
    // strategy answers `429` for the very same request.
    let now = start();
    let values = values("t1", None, None, None);
    let queue = {
        let mut config = config(2, RateLimitWindow::Second, Some(1), SharingMode::Private);
        config.strategy = RateLimitStrategy::Queue;
        EffectiveRateLimit::from_config(&config)
    };
    let reject = {
        let mut config = config(2, RateLimitWindow::Second, Some(1), SharingMode::Private);
        config.strategy = RateLimitStrategy::Reject;
        EffectiveRateLimit::from_config(&config)
    };

    let queue_registry = RateLimiterRegistry::new();
    assert!(
        queue_registry
            .check(UPSTREAM, &queue, &values, now, 1_000)
            .is_allowed()
    );
    let second = queue_registry.check(UPSTREAM, &queue, &values, now, 1_000);
    assert!(second.is_allowed(), "the queued request is admitted");
    let wait = second.queue_wait.expect("the queued request waits");
    assert!(wait > Duration::ZERO, "the wait is the token refill time");
    assert!(wait <= QUEUE_MAX_WAIT.min(Duration::from_millis(600)));

    let reject_registry = RateLimiterRegistry::new();
    assert!(
        reject_registry
            .check(UPSTREAM, &reject, &values, now, 1_000)
            .is_allowed()
    );
    let second = reject_registry.check(UPSTREAM, &reject, &values, now, 1_000);
    assert!(second.is_limited(), "the same request is refused");
    assert!(second.queue_wait.is_none());
}

#[test]
fn a_recreated_upstream_starts_from_an_empty_budget() {
    // `clear_upstream` is what the store calls when an upstream is deleted: the
    // buckets of the *deleted* id are gone, so a new upstream (new id, same
    // alias) cannot inherit the budget its predecessor already spent.
    let registry = RateLimiterRegistry::new();
    let effective = effective(1, RateLimitWindow::Second, Some(1));
    let now = start();
    let values = values("t1", None, None, None);
    let deleted = Uuid::from_u128(0x99);
    let recreated = Uuid::from_u128(0xaa);

    assert!(
        registry
            .check(deleted, &effective, &values, now, 1_000)
            .is_allowed()
    );
    assert!(
        registry
            .check(deleted, &effective, &values, now, 1_000)
            .is_limited(),
        "the old upstream's budget is spent"
    );
    assert!(
        registry
            .check(recreated, &effective, &values, now, 1_000)
            .is_allowed(),
        "a different id is a different bucket"
    );
    registry.clear_upstream(deleted);
    assert_eq!(registry.len(), 1, "only the deleted upstream's buckets go");
    assert!(
        registry
            .check(deleted, &effective, &values, now, 1_000)
            .is_allowed(),
        "the dropped bucket starts full again"
    );
}

#[test]
fn the_registry_is_bounded_and_evicts_the_least_recently_used() {
    // One key per upstream, in increasing touch order: once the bound is
    // reached the *oldest* buckets are the ones that have to go.
    let registry = RateLimiterRegistry::new();
    let effective = effective(1, RateLimitWindow::Second, Some(1));
    let base = start();
    let total = MAX_BUCKETS + MAX_BUCKETS / 8;
    for index in 0..total {
        let upstream = Uuid::from_u128(index as u128 + 1);
        let now = base + Duration::from_micros(index as u64 + 1);
        let _ = registry.check(
            upstream,
            &effective,
            &values("t1", None, None, None),
            now,
            1_000,
        );
    }
    assert!(
        registry.len() <= MAX_BUCKETS,
        "the registry never grows past its bound ({} live buckets)",
        registry.len()
    );
    let values = values("t1", None, None, None);
    let oldest = registry.scope_key(Uuid::from_u128(1), RateLimitScope::Tenant, &values);
    assert!(
        !registry.limiters.contains_key(&oldest),
        "the least recently used bucket is evicted first"
    );
    let newest = registry.scope_key(
        Uuid::from_u128(total as u128),
        RateLimitScope::Tenant,
        &values,
    );
    assert!(
        registry.limiters.contains_key(&newest),
        "the most recently used bucket survives the eviction"
    );
}

#[test]
fn degrade_strategy_serves_and_flags_the_response() {
    let registry = RateLimiterRegistry::new();
    let mut config = config(1, RateLimitWindow::Second, Some(1), SharingMode::Private);
    config.strategy = RateLimitStrategy::Degrade;
    let effective = EffectiveRateLimit::from_config(&config);
    let now = start();
    let epoch = 1_000;
    let first = registry.check(
        UPSTREAM,
        &effective,
        &values("t1", None, None, None),
        now,
        epoch,
    );
    assert!(first.is_allowed());
    assert!(!first.degraded);
    let second = registry.check(
        UPSTREAM,
        &effective,
        &values("t1", None, None, None),
        now,
        epoch,
    );
    assert!(second.is_allowed(), "degrade never rejects");
    assert!(second.degraded);
    let headers = second.headers();
    assert_eq!(
        header_value(&headers, DEGRADED_HEADER).as_deref(),
        Some(DEGRADED_HEADER_VALUE)
    );
}

#[test]
fn sliding_window_algorithm_is_honoured_by_the_registry() {
    let registry = RateLimiterRegistry::new();
    let mut config = config(2, RateLimitWindow::Second, None, SharingMode::Private);
    config.algorithm = RateLimitAlgorithm::SlidingWindow;
    let effective = EffectiveRateLimit::from_config(&config);
    let now = start();
    let epoch = 1_000;
    let first = registry.check(
        UPSTREAM,
        &effective,
        &values("t1", None, None, None),
        now,
        epoch,
    );
    let second = registry.check(
        UPSTREAM,
        &effective,
        &values("t1", None, None, None),
        now,
        epoch,
    );
    let third = registry.check(
        UPSTREAM,
        &effective,
        &values("t1", None, None, None),
        now,
        epoch,
    );
    assert!(first.is_allowed());
    assert!(second.is_allowed());
    assert!(third.is_limited());
}

#[test]
fn clearing_an_upstream_drops_only_its_limiters() {
    let registry = RateLimiterRegistry::new();
    let effective = effective(1, RateLimitWindow::Second, Some(1));
    let now = start();
    let _ = registry.check(
        UPSTREAM,
        &effective,
        &values("t1", None, None, None),
        now,
        1_000,
    );
    let _ = registry.check(
        Uuid::from_u128(0x99),
        &effective,
        &values("t1", None, None, None),
        now,
        1_000,
    );
    assert_eq!(registry.len(), 2);
    registry.clear_upstream(Uuid::from_u128(0x99));
    assert_eq!(registry.len(), 1);
    registry.clear();
    assert!(registry.is_empty());
}

#[test]
fn a_configured_capacity_change_resets_the_counter() {
    let registry = RateLimiterRegistry::new();
    let now = start();
    let first = effective(1, RateLimitWindow::Second, Some(1));
    let _granted = registry.check(
        UPSTREAM,
        &first,
        &values("t1", None, None, None),
        now,
        1_000,
    );
    let second = effective(1, RateLimitWindow::Second, Some(5));
    let granted = registry.check(
        UPSTREAM,
        &second,
        &values("t1", None, None, None),
        now,
        1_000,
    );
    assert!(granted.is_allowed());
    assert_eq!(granted.limit, 5);
}

// ---------------------------------------------------------------------------
// Queue reservations and their refunds
// ---------------------------------------------------------------------------

#[test]
fn a_denied_queue_reservation_still_carries_the_retry_after() {
    // A sliding window is a log of hits, so a reservation inside the queue
    // bound cannot always be delivered: at capacity there is no hit to record
    // and the request is denied even though the projected wait is short. That
    // answer is a `429` all the same, so it carries the ADR-0003 `Retry-After`
    // like a rejection past the bound does.
    let registry = RateLimiterRegistry::new();
    let mut config = config(2, RateLimitWindow::Second, Some(2), SharingMode::Private);
    config.algorithm = RateLimitAlgorithm::SlidingWindow;
    config.strategy = RateLimitStrategy::Queue;
    let effective = EffectiveRateLimit::from_config(&config);
    let now = start();
    let values = values("t1", None, None, None);
    let first = registry.check(UPSTREAM, &effective, &values, now, 1_000);
    let second = registry.check(UPSTREAM, &effective, &values, now, 1_000);
    assert!(first.is_allowed() && second.is_allowed());
    assert!(first.queue_wait.is_none() && second.queue_wait.is_none());

    let denied = registry.check(UPSTREAM, &effective, &values, now, 1_000);
    assert!(denied.is_limited(), "the window is at capacity");
    // The oldest of the two hits expires one second after it was recorded,
    // inside the queue bound, so the request was *queued* and then denied.
    assert_eq!(denied.queue_wait, Some(Duration::from_secs(1)));
    assert!(denied.queue_wait.unwrap() <= QUEUE_MAX_WAIT);
    assert_eq!(denied.retry_after_seconds, Some(1));
    let headers = denied.headers();
    assert_eq!(
        header_value(&headers, RETRY_AFTER_HEADER).as_deref(),
        Some("1")
    );
    let error = denied.into_error();
    assert_eq!(error.status(), axum::http::StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(error.problem_body().context.retry_after_seconds, Some(1));
}

#[test]
fn sliding_window_release_drops_the_newest_hit_of_that_cost() {
    let now = start();
    let mut window = SlidingWindow::new(3.0, Duration::from_secs(10));
    assert!(window.try_acquire(1.0, now));
    assert!(window.try_acquire(2.0, now));
    assert!(!window.try_acquire(1.0, now), "the window is at capacity");

    window.release(2.0, now);
    assert_eq!(
        window.remaining(now),
        2.0,
        "the refunded cost is free again"
    );
    window.release(1.0, now);
    assert_eq!(window.remaining(now), 3.0, "the window is empty again");
    window.release(1.0, now);
    assert_eq!(
        window.remaining(now),
        3.0,
        "a refund never credits a balance that was not charged"
    );
}

#[test]
fn token_bucket_release_restores_the_balance_without_minting_tokens() {
    let now = start();
    let mut bucket = TokenBucket::new(2.0, 1.0, now);
    assert!(bucket.try_acquire(2.0, now));
    assert!(!bucket.try_acquire(1.0, now), "the bucket is empty");

    bucket.release(2.0, now);
    assert_eq!(bucket.tokens_at(now), 2.0);

    // The refund clamps to the capacity, so a request that failed after the
    // bucket already refilled cannot turn its debit into extra budget.
    bucket.release(2.0, now + Duration::from_secs(3));
    assert_eq!(bucket.tokens_at(now + Duration::from_secs(3)), 2.0);
}

#[test]
fn a_failed_queued_request_puts_its_tokens_back() {
    // Two identical buckets, each spending one token through the `queue`
    // strategy for a request that then fails. Only the first gets the refund,
    // so only its next request waits for a single token again instead of two.
    let queue = || {
        let mut config = config(2, RateLimitWindow::Second, Some(1), SharingMode::Private);
        config.strategy = RateLimitStrategy::Queue;
        EffectiveRateLimit::from_config(&config)
    };
    let effective = queue();
    let now = start();
    let values = values("t1", None, None, None);
    let spend = |registry: &RateLimiterRegistry| {
        let granted = registry.check(UPSTREAM, &effective, &values, now, 1_000);
        assert!(granted.is_allowed(), "the bucket starts full");
        assert_eq!(granted.queue_wait, None);
        let queued = registry.check(UPSTREAM, &effective, &values, now, 1_000);
        assert_eq!(
            queued.queue_wait,
            Some(Duration::from_millis(500)),
            "the queued request debits the token it waits for"
        );
        registry
            .reservation(UPSTREAM, &effective, &values, &queued)
            .expect("a queued request holds a reservation")
    };

    let refunded = RateLimiterRegistry::new();
    refunded.release(spend(&refunded), now);
    let after_refund = refunded.check(UPSTREAM, &effective, &values, now, 1_000);
    assert_eq!(
        after_refund.queue_wait,
        Some(Duration::from_millis(500)),
        "the bucket is back at the balance it had before the debit"
    );

    let kept = RateLimiterRegistry::new();
    drop(spend(&kept));
    let without_refund = kept.check(UPSTREAM, &effective, &values, now, 1_000);
    assert_eq!(
        without_refund.queue_wait,
        Some(Duration::from_secs(1)),
        "an unreleased debit is still held"
    );
}

#[test]
fn a_denied_request_holds_no_reservation_to_refund() {
    // A sliding window cannot go into credit: its `429` from a full queue
    // carries the projected wait but recorded no hit, so it captures no
    // reservation — a handle for it would pay back a hit another request made.
    let registry = RateLimiterRegistry::new();
    let mut config = config(2, RateLimitWindow::Second, Some(2), SharingMode::Private);
    config.algorithm = RateLimitAlgorithm::SlidingWindow;
    config.strategy = RateLimitStrategy::Queue;
    let effective = EffectiveRateLimit::from_config(&config);
    let now = start();
    let values = values("t1", None, None, None);
    let first = registry.check(UPSTREAM, &effective, &values, now, 1_000);
    let second = registry.check(UPSTREAM, &effective, &values, now, 1_000);
    assert!(first.is_allowed() && second.is_allowed());
    assert!(first.queue_wait.is_none() && second.queue_wait.is_none());

    let denied = registry.check(UPSTREAM, &effective, &values, now, 1_000);
    assert!(denied.is_limited());
    assert!(denied.queue_wait.is_some());
    assert!(
        registry
            .reservation(UPSTREAM, &effective, &values, &denied)
            .is_none()
    );
    assert!(
        registry
            .check(UPSTREAM, &effective, &values, now, 1_000)
            .is_limited(),
        "the two granted hits are still held"
    );
}
