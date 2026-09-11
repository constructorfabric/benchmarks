//! The per-Data-Plane-instance token-bucket engine
//! (`cpt-cf-oagw-dod-ratelimit-position`, `cpt-cf-oagw-dod-ratelimit-concurrency`)
//! and the `strategy` dispatch that turns one
//! `cpt-cf-oagw-algo-ratelimit-consume` outcome into the documented
//! `429`/forward/queue behaviour (`cpt-cf-oagw-dod-ratelimit-rejection-contract`).
//!
//! Counters are owned in-process, per instance, in a `dashmap::DashMap`
//! keyed by [`CounterKey`]: no cross-instance synchronization is
//! implemented, per `cpt-cf-oagw-adr-state-management`'s Data-Plane-owns-
//! rate-limiters decision and this feature's documented out-of-scope Redis
//! sync mode.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::response::Response;
use dashmap::DashMap;

use crate::error::{OagwError, OagwErrorKind};
use crate::model::upstream::RateLimitStrategy;
use crate::policy::ratelimit::bucket::{self, ConsumeDecision, ConsumeOutcome, TokenBucketState};
use crate::policy::ratelimit::clock::Clock;
use crate::policy::ratelimit::headers::{self, RateLimitHeaders};
use crate::policy::ratelimit::key::CounterKey;
use crate::policy::ratelimit::limit::EffectiveRateLimit;

/// The bounded wait/queue-depth constants for `strategy: queue`
/// (`inst-ratelimit-exceeded-07`/`-08`/`-09`): no specific values are
/// documented by the FEATURE beyond "bounded", so this module picks a
/// latency budget consistent with `cpt-cf-oagw-nfr-low-latency`'s <10ms p95
/// (a handful of short polls) and a small per-key concurrent-waiter cap.
/// Reachable only through [`admit_with_queue`], which the current adapter
/// (`crate::policy::ratelimit::evaluate_budget`) cannot call -- see this
/// feature's manifest `richer_entry_point` note.
#[allow(dead_code)]
const QUEUE_MAX_WAIT: Duration = Duration::from_millis(50);
#[allow(dead_code)]
const QUEUE_POLL_INTERVAL: Duration = Duration::from_millis(5);
#[allow(dead_code)]
const QUEUE_MAX_DEPTH: usize = 32;

/// Owns every token bucket and every per-key queue-depth counter for one
/// Data-Plane instance.
pub(crate) struct RateLimitEngine {
    buckets: DashMap<CounterKey, TokenBucketState>,
    /// Only touched by [`admit_with_queue`]'s `strategy: queue` path;
    /// dead code under today's adapter for the same reason as
    /// `QUEUE_MAX_WAIT` above.
    #[allow(dead_code)]
    queue_depth: DashMap<CounterKey, usize>,
}

impl Default for RateLimitEngine {
    fn default() -> Self {
        Self {
            buckets: DashMap::new(),
            queue_depth: DashMap::new(),
        }
    }
}

impl RateLimitEngine {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// `cpt-cf-oagw-algo-ratelimit-consume`, steps 1-9, run as one atomic
    /// unit per key (`inst-ratelimit-consume`'s closing note,
    /// `cpt-cf-oagw-dod-ratelimit-concurrency`): `dashmap`'s per-shard
    /// locking on the `entry` API keeps the seed-or-fetch and the
    /// read-modify-write for one key from ever interleaving with another
    /// evaluation of that same key.
    // @cpt-dod:cpt-cf-oagw-dod-ratelimit-concurrency:p1
    // @cpt-begin:cpt-cf-oagw-algo-ratelimit-consume:p1:inst-ratelimit-consume-01
    // @cpt-begin:cpt-cf-oagw-algo-ratelimit-consume:p1:inst-ratelimit-consume-02
    // @cpt-begin:cpt-cf-oagw-algo-ratelimit-consume:p1:inst-ratelimit-consume-12
    // @cpt-begin:cpt-cf-oagw-algo-ratelimit-consume:p1:inst-ratelimit-consume-13
    pub(crate) fn consume(
        &self,
        key: CounterKey,
        config: &EffectiveRateLimit,
        clock: &dyn Clock,
    ) -> ConsumeOutcome {
        let now = clock.now();
        let mut entry = self
            .buckets
            .entry(key)
            .or_insert_with(|| TokenBucketState::seeded(config.burst_capacity, now));
        bucket::consume(&mut entry, config.rate_per_second, config.cost, now)
    }
    // @cpt-end:cpt-cf-oagw-algo-ratelimit-consume:p1:inst-ratelimit-consume-13
    // @cpt-end:cpt-cf-oagw-algo-ratelimit-consume:p1:inst-ratelimit-consume-12
    // @cpt-end:cpt-cf-oagw-algo-ratelimit-consume:p1:inst-ratelimit-consume-02
    // @cpt-end:cpt-cf-oagw-algo-ratelimit-consume:p1:inst-ratelimit-consume-01

    #[cfg(test)]
    pub(crate) fn bucket_count(&self) -> usize {
        self.buckets.len()
    }
}

/// What a strategy decided to do with one request, carrying the headers
/// `cpt-cf-oagw-algo-ratelimit-headers` computed either way.
pub(crate) enum StrategyOutcome {
    Forward(RateLimitHeaders),
    Reject(RateLimitHeaders),
}

fn wall_clock_unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since_epoch| since_epoch.as_secs())
        .unwrap_or(0)
}

fn headers_for(
    outcome: ConsumeOutcome,
    config: &EffectiveRateLimit,
    now: Instant,
) -> RateLimitHeaders {
    headers::compute_headers(
        outcome.decision,
        outcome.remaining,
        // `X-RateLimit-Limit` must share a scale with `X-RateLimit-Remaining`,
        // which counts tokens left in the bucket. ADR-0003's example pairs
        // `Limit: 100` with `Remaining: 0`, so the limit is the bucket
        // capacity -- NOT `display_rate` (`floor(rate_per_second)`), which for
        // e.g. `sustained: {rate: 100, window: minute}` is `1` and would
        // advertise `Limit: 1, Remaining: 99`.
        config.burst_capacity,
        outcome.reset_at,
        now,
        wall_clock_unix_now(),
        config.strategy,
    )
}

/// `cpt-cf-oagw-flow-ratelimit-within-limit` / `cpt-cf-oagw-flow-ratelimit-exceeded`,
/// `strategy: reject` and `strategy: degrade` halves: both are a single
/// synchronous `consume` call away from a decision, unlike `queue`
/// (`admit_with_queue`), which may retry.
// @cpt-flow:cpt-cf-oagw-flow-ratelimit-within-limit:p1
// @cpt-begin:cpt-cf-oagw-flow-ratelimit-within-limit:p1:inst-ratelimit-within-limit-06
// @cpt-begin:cpt-cf-oagw-flow-ratelimit-within-limit:p1:inst-ratelimit-within-limit-07
// @cpt-begin:cpt-cf-oagw-flow-ratelimit-within-limit:p1:inst-ratelimit-within-limit-08
// @cpt-begin:cpt-cf-oagw-flow-ratelimit-within-limit:p1:inst-ratelimit-within-limit-09
// @cpt-begin:cpt-cf-oagw-flow-ratelimit-exceeded:p1:inst-ratelimit-exceeded-11
// @cpt-begin:cpt-cf-oagw-flow-ratelimit-exceeded:p1:inst-ratelimit-exceeded-12
pub(crate) fn evaluate_reject_or_degrade(
    engine: &RateLimitEngine,
    key: CounterKey,
    config: &EffectiveRateLimit,
    clock: &dyn Clock,
) -> StrategyOutcome {
    let now = clock.now();
    // @cpt-begin:cpt-cf-oagw-flow-ratelimit-exceeded:p1:inst-ratelimit-exceeded-02
    let outcome = engine.consume(key, config, clock);
    // @cpt-end:cpt-cf-oagw-flow-ratelimit-exceeded:p1:inst-ratelimit-exceeded-02
    let headers = headers_for(outcome, config, now);
    match (outcome.decision, config.strategy) {
        // `strategy: degrade` on DENY: forward anyway (never consuming
        // further tokens, since `consume` already left them untouched on a
        // DENY), marked by `headers`'s `X-RateLimit-Remaining: 0`.
        (ConsumeDecision::Allow, _) | (ConsumeDecision::Deny, RateLimitStrategy::Degrade) => {
            StrategyOutcome::Forward(headers)
        }
        (ConsumeDecision::Deny, _) => StrategyOutcome::Reject(headers),
    }
}
// @cpt-end:cpt-cf-oagw-flow-ratelimit-exceeded:p1:inst-ratelimit-exceeded-12
// @cpt-end:cpt-cf-oagw-flow-ratelimit-exceeded:p1:inst-ratelimit-exceeded-11
// @cpt-end:cpt-cf-oagw-flow-ratelimit-within-limit:p1:inst-ratelimit-within-limit-09
// @cpt-end:cpt-cf-oagw-flow-ratelimit-within-limit:p1:inst-ratelimit-within-limit-08
// @cpt-end:cpt-cf-oagw-flow-ratelimit-within-limit:p1:inst-ratelimit-within-limit-07
// @cpt-end:cpt-cf-oagw-flow-ratelimit-within-limit:p1:inst-ratelimit-within-limit-06

/// `strategy: queue` (`inst-ratelimit-exceeded-06` through `-10`): on
/// exhaustion, hold the request in a small bounded per-key "queue" (really
/// a waiter count plus a short poll loop against the same bucket) until
/// either a token frees up or `QUEUE_MAX_WAIT` elapses; a key already at
/// `QUEUE_MAX_DEPTH` concurrent waiters is rejected immediately under the
/// same `429` contract as `strategy: reject`, matching the FEATURE's
/// documented Error Scenario.
// @cpt-flow:cpt-cf-oagw-flow-ratelimit-exceeded:p1
// @cpt-begin:cpt-cf-oagw-flow-ratelimit-exceeded:p1:inst-ratelimit-exceeded-06
// @cpt-begin:cpt-cf-oagw-flow-ratelimit-exceeded:p1:inst-ratelimit-exceeded-07
// @cpt-begin:cpt-cf-oagw-flow-ratelimit-exceeded:p1:inst-ratelimit-exceeded-08
// @cpt-begin:cpt-cf-oagw-flow-ratelimit-exceeded:p1:inst-ratelimit-exceeded-09
// @cpt-begin:cpt-cf-oagw-flow-ratelimit-exceeded:p1:inst-ratelimit-exceeded-10
// Reachable only from this module's own tests today: the fixed adapter
// (`crate::policy::ratelimit::evaluate_budget`) has no `strategy` input to
// route through to `queue`. See this feature's manifest `richer_entry_point`.
#[allow(dead_code)]
pub(crate) async fn admit_with_queue(
    engine: &RateLimitEngine,
    key: CounterKey,
    config: &EffectiveRateLimit,
    clock: &dyn Clock,
) -> StrategyOutcome {
    {
        let mut depth = engine.queue_depth.entry(key.clone()).or_insert(0);
        if *depth >= QUEUE_MAX_DEPTH {
            let now = clock.now();
            let outcome = engine.consume(key, config, clock);
            return StrategyOutcome::Reject(headers_for(outcome, config, now));
        }
        *depth += 1;
    }

    let result = poll_until_admitted_or_timeout(engine, &key, config, clock).await;

    if let Some(mut depth) = engine.queue_depth.get_mut(&key) {
        *depth = depth.saturating_sub(1);
    }
    result
}
// @cpt-end:cpt-cf-oagw-flow-ratelimit-exceeded:p1:inst-ratelimit-exceeded-10
// @cpt-end:cpt-cf-oagw-flow-ratelimit-exceeded:p1:inst-ratelimit-exceeded-09
// @cpt-end:cpt-cf-oagw-flow-ratelimit-exceeded:p1:inst-ratelimit-exceeded-08
// @cpt-end:cpt-cf-oagw-flow-ratelimit-exceeded:p1:inst-ratelimit-exceeded-07
// @cpt-end:cpt-cf-oagw-flow-ratelimit-exceeded:p1:inst-ratelimit-exceeded-06

#[allow(dead_code)]
async fn poll_until_admitted_or_timeout(
    engine: &RateLimitEngine,
    key: &CounterKey,
    config: &EffectiveRateLimit,
    clock: &dyn Clock,
) -> StrategyOutcome {
    let deadline = clock.now() + QUEUE_MAX_WAIT;
    loop {
        let now = clock.now();
        let outcome = engine.consume(key.clone(), config, clock);
        if outcome.decision == ConsumeDecision::Allow {
            return StrategyOutcome::Forward(headers_for(outcome, config, now));
        }
        if clock.now() >= deadline {
            return StrategyOutcome::Reject(headers_for(outcome, config, now));
        }
        tokio::time::sleep(QUEUE_POLL_INTERVAL).await;
    }
}

/// `cpt-cf-oagw-dod-ratelimit-rejection-contract`: render the documented
/// `429` envelope carrying `Retry-After` (via `OagwError`) and the
/// `X-RateLimit-*` headers, never forwarding to the upstream.
// @cpt-dod:cpt-cf-oagw-dod-ratelimit-rejection-contract:p1
// @cpt-begin:cpt-cf-oagw-flow-ratelimit-exceeded:p1:inst-ratelimit-exceeded-04
// @cpt-begin:cpt-cf-oagw-flow-ratelimit-exceeded:p1:inst-ratelimit-exceeded-05
pub(crate) fn reject_response(headers: &RateLimitHeaders) -> Response {
    use axum::response::IntoResponse;

    let mut error = OagwError::new(
        OagwErrorKind::RateLimitExceeded,
        "the rate limit for this resource has been exceeded",
    );
    if let Some(retry_after) = headers.retry_after_secs {
        error = error.with_retry_after_seconds(retry_after);
    }
    let mut response = error.into_response();
    headers.apply(response.headers_mut());
    response
}
// @cpt-end:cpt-cf-oagw-flow-ratelimit-exceeded:p1:inst-ratelimit-exceeded-05
// @cpt-end:cpt-cf-oagw-flow-ratelimit-exceeded:p1:inst-ratelimit-exceeded-04

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::model::upstream::{RateLimitAlgorithm, RateLimitScope};
    use crate::policy::ratelimit::clock::SystemClock;
    use crate::policy::ratelimit::clock::test_support::ManualClock;
    use crate::policy::ratelimit::key::{
        CounterKey, ResourceRef, ScopeContext, select_counter_key,
    };
    use std::sync::Arc;
    use uuid::Uuid;

    fn config(
        rate_per_second: f64,
        burst_capacity: u32,
        strategy: RateLimitStrategy,
    ) -> EffectiveRateLimit {
        EffectiveRateLimit {
            algorithm: RateLimitAlgorithm::TokenBucket,
            rate_per_second,
            display_rate: rate_per_second.floor() as u32,
            burst_capacity,
            scope: RateLimitScope::Tenant,
            strategy,
            cost: 1,
        }
    }

    /// `X-RateLimit-Limit` is the bucket capacity, so it is always on the
    /// same scale as `X-RateLimit-Remaining`. Regression test for a live
    /// defect found against the running server: a
    /// `sustained: {rate: 100, window: minute}` upstream advertised
    /// `Limit: 1, Remaining: 99`, because the limit came from
    /// `display_rate` (`floor(100/60) == 1`) while `remaining` counts
    /// bucket tokens. ADR-0003 pairs `Limit: 100` with `Remaining: 0`.
    #[test]
    fn limit_header_is_the_bucket_capacity_so_remaining_never_exceeds_it() {
        // 100 requests per minute => rate_per_second 1.67, display_rate 1.
        let config = config(100.0 / 60.0, 100, RateLimitStrategy::Reject);
        assert_eq!(config.display_rate, 1, "precondition: the floored rate");

        let engine = RateLimitEngine::new();
        let clock = ManualClock::new();
        let outcome = engine.consume(some_key(), &config, &clock);
        let headers = headers_for(outcome, &config, clock.now());

        assert_eq!(
            headers.limit, 100,
            "Limit must be the bucket capacity, not floor(rate_per_second)"
        );
        assert_eq!(headers.remaining, 99);
        assert!(
            headers.remaining <= headers.limit,
            "Remaining ({}) must never exceed Limit ({})",
            headers.remaining,
            headers.limit
        );
    }

    fn some_key() -> CounterKey {
        let ctx = ScopeContext {
            contributing_resource: ResourceRef::Upstream(Uuid::new_v4()),
            route_id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            principal_id: Uuid::new_v4(),
            client_ip: None,
        };
        select_counter_key(RateLimitScope::Tenant, &ctx)
    }

    #[test]
    fn requests_within_capacity_are_forwarded_with_decreasing_remaining() {
        let engine = RateLimitEngine::new();
        let clock = ManualClock::new();
        let config = config(1.0, 3, RateLimitStrategy::Reject);
        let key = some_key();

        for expected_remaining in [2u32, 1, 0] {
            match evaluate_reject_or_degrade(&engine, key.clone(), &config, &clock) {
                StrategyOutcome::Forward(headers) => {
                    assert_eq!(headers.remaining, expected_remaining);
                }
                StrategyOutcome::Reject(_) => panic!("expected forward within capacity"),
            }
        }
    }

    #[test]
    fn reject_strategy_denies_with_retry_after_once_exhausted() {
        let engine = RateLimitEngine::new();
        let clock = ManualClock::new();
        let config = config(1.0, 1, RateLimitStrategy::Reject);
        let key = some_key();

        assert!(matches!(
            evaluate_reject_or_degrade(&engine, key.clone(), &config, &clock),
            StrategyOutcome::Forward(_)
        ));
        match evaluate_reject_or_degrade(&engine, key, &config, &clock) {
            StrategyOutcome::Reject(headers) => {
                assert_eq!(headers.remaining, 0);
                assert!(headers.retry_after_secs.is_some());
            }
            StrategyOutcome::Forward(_) => panic!("expected reject once exhausted"),
        }
    }

    #[test]
    fn degrade_strategy_forwards_with_remaining_zero_once_exhausted() {
        let engine = RateLimitEngine::new();
        let clock = ManualClock::new();
        let config = config(1.0, 1, RateLimitStrategy::Degrade);
        let key = some_key();

        assert!(matches!(
            evaluate_reject_or_degrade(&engine, key.clone(), &config, &clock),
            StrategyOutcome::Forward(_)
        ));
        match evaluate_reject_or_degrade(&engine, key, &config, &clock) {
            StrategyOutcome::Forward(headers) => {
                assert_eq!(headers.remaining, 0);
                assert!(headers.retry_after_secs.is_none());
            }
            StrategyOutcome::Reject(_) => panic!("degrade must still forward"),
        }
    }

    #[test]
    fn replenishment_after_the_window_admits_a_further_request() {
        let engine = RateLimitEngine::new();
        let clock = ManualClock::new();
        let config = config(1.0, 1, RateLimitStrategy::Reject);
        let key = some_key();

        assert!(matches!(
            evaluate_reject_or_degrade(&engine, key.clone(), &config, &clock),
            StrategyOutcome::Forward(_)
        ));
        assert!(matches!(
            evaluate_reject_or_degrade(&engine, key.clone(), &config, &clock),
            StrategyOutcome::Reject(_)
        ));

        clock.advance(Duration::from_secs(2));
        assert!(matches!(
            evaluate_reject_or_degrade(&engine, key, &config, &clock),
            StrategyOutcome::Forward(_)
        ));
    }

    #[test]
    fn burst_capacity_permits_a_spike_faster_than_the_sustained_rate() {
        let engine = RateLimitEngine::new();
        let clock = ManualClock::new();
        // Sustained rate is slow (1 token per 10s), but a burst of 5 all
        // succeed immediately against a freshly-seeded bucket.
        let config = config(0.1, 5, RateLimitStrategy::Reject);
        let key = some_key();

        for _ in 0..5 {
            assert!(matches!(
                evaluate_reject_or_degrade(&engine, key.clone(), &config, &clock),
                StrategyOutcome::Forward(_)
            ));
        }
        assert!(matches!(
            evaluate_reject_or_degrade(&engine, key, &config, &clock),
            StrategyOutcome::Reject(_)
        ));
    }

    #[test]
    fn reject_response_carries_the_documented_status_type_and_headers() {
        let headers = RateLimitHeaders {
            limit: 10,
            remaining: 0,
            reset_unix: 1_700_000_010,
            retry_after_secs: Some(10),
        };
        let response = reject_response(&headers);
        assert_eq!(response.status().as_u16(), 429);
        assert_eq!(
            response
                .headers()
                .get(crate::error::ERROR_SOURCE_HEADER_NAME)
                .and_then(|v| v.to_str().ok()),
            Some(crate::error::ERROR_SOURCE_GATEWAY)
        );
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some("10")
        );
        assert_eq!(
            response
                .headers()
                .get(headers::HEADER_REMAINING)
                .and_then(|v| v.to_str().ok()),
            Some("0")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_consumption_never_over_admits_beyond_burst_capacity() {
        let engine = Arc::new(RateLimitEngine::new());
        let clock = Arc::new(ManualClock::new());
        let config = Arc::new(config(0.0, 20, RateLimitStrategy::Reject));
        let key = some_key();

        let mut tasks = Vec::new();
        for _ in 0..200 {
            let engine = Arc::clone(&engine);
            let clock = Arc::clone(&clock);
            let config = Arc::clone(&config);
            let key = key.clone();
            tasks.push(tokio::spawn(async move {
                matches!(
                    evaluate_reject_or_degrade(&engine, key, &config, clock.as_ref()),
                    StrategyOutcome::Forward(_)
                )
            }));
        }

        let mut admitted = 0usize;
        for task in tasks {
            if task.await.unwrap() {
                admitted += 1;
            }
        }
        assert_eq!(admitted, 20);
        // Exactly one bucket exists for the key despite 200 concurrent
        // first-touches, confirming the seed-or-fetch itself never races.
        assert_eq!(engine.bucket_count(), 1);
    }

    #[tokio::test]
    async fn queue_strategy_admits_once_a_token_frees_up_within_the_bounded_wait() {
        // Real time here (not the manual clock): the queue's bounded wait is
        // itself a real `tokio::time::sleep` poll loop, so the replenishment
        // it waits on must be measured against the same real clock.
        let engine = RateLimitEngine::new();
        let clock = SystemClock;
        // Fast enough that one `QUEUE_POLL_INTERVAL` tick replenishes well
        // over the single token this test needs.
        let config = config(1000.0, 1, RateLimitStrategy::Queue);
        let key = some_key();

        assert!(matches!(
            evaluate_reject_or_degrade(&engine, key.clone(), &config, &clock),
            StrategyOutcome::Forward(_)
        ));

        let outcome = admit_with_queue(&engine, key, &config, &clock).await;
        assert!(matches!(outcome, StrategyOutcome::Forward(_)));
    }

    #[tokio::test]
    async fn queue_strategy_rejects_once_the_bounded_queue_is_full() {
        let engine = RateLimitEngine::new();
        let clock = SystemClock;
        // Never replenishes, so every waiter times out and falls back to
        // the reject contract; exhaust the bounded queue depth first.
        let config = config(0.0, 1, RateLimitStrategy::Queue);
        let key = some_key();

        assert!(matches!(
            evaluate_reject_or_degrade(&engine, key.clone(), &config, &clock),
            StrategyOutcome::Forward(_)
        ));

        engine.queue_depth.insert(key.clone(), QUEUE_MAX_DEPTH);

        let outcome = admit_with_queue(&engine, key, &config, &clock).await;
        assert!(matches!(outcome, StrategyOutcome::Reject(_)));
    }
}
