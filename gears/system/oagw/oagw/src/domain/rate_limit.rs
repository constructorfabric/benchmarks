//! Token-bucket rate limiting: effective-limit derivation, counter-key
//! construction, per-instance admission, and the advisory response headers
//! (`cpt-cf-oagw-algo-effective-rate-limit`, `cpt-cf-oagw-algo-token-bucket-admission`,
//! `cpt-cf-oagw-algo-rate-limit-headers`, `cpt-cf-oagw-dod-effective-rate-limit`,
//! `cpt-cf-oagw-dod-token-bucket-admission`, `cpt-cf-oagw-dod-rate-limit-response`).
//!
//! This module implements ONLY the token-bucket algorithm (the configuration
//! default); the optional sliding-window algorithm is not implemented, and
//! `queue`/`degrade` strategies fall back to `reject` semantics by never
//! branching on [`crate::domain::model::Strategy`] at all
//! (controller decision D12) — every exhausted bucket is rejected the same
//! way regardless of the configured strategy value.
//!
//! Counters are per-instance, in-process, and lost on restart
//! (`cpt-cf-oagw-state-token-bucket`): [`RateLimiter`] owns a
//! [`dashmap::DashMap`] keyed by the counter key this module builds, with no
//! cross-node synchronization (Overview override 5).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use uuid::Uuid;

use super::model::{RateLimitConfig, RateLimitScope, Strategy};
use super::resolve::window_seconds;

/// The effective, already-normalised rate-limit inputs for one request
/// (`cpt-cf-oagw-algo-effective-rate-limit`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EffectiveRateLimit {
    /// Tokens replenished per second, normalised from `sustained.rate` and
    /// `sustained.window`.
    pub replenish_per_sec: f64,
    /// Bucket capacity: `burst.capacity`, defaulting to `sustained.rate`.
    pub burst_capacity: f64,
    /// Tokens consumed by an admitted request.
    pub cost: f64,
    /// The raw configured sustained rate, used verbatim for the
    /// `X-RateLimit-Limit` header (`cpt-cf-oagw-algo-rate-limit-headers`).
    pub sustained_rate: u32,
    pub scope: RateLimitScope,
    /// Read but never branched on: `queue` and `degrade` are honoured by
    /// falling back to the same reject semantics as `Strategy::Reject`
    /// (controller decision D12).
    pub strategy: Strategy,
}

/// Derives [`EffectiveRateLimit`] from the single merged `rate_limit` value
/// configuration resolution already produced, without recomputing any
/// sharing mode or hierarchy-wide reduction
/// (`cpt-cf-oagw-algo-effective-rate-limit`, `cpt-cf-oagw-dod-effective-rate-limit`).
#[must_use]
pub fn effective_rate_limit(config: &RateLimitConfig) -> EffectiveRateLimit {
    // `window_seconds` returns `u32` specifically so this conversion is
    // lossless (see its doc comment): every window value fits `f64` exactly.
    let window_secs = f64::from(window_seconds(config.sustained.window));
    let replenish_per_sec = f64::from(config.sustained.rate) / window_secs;
    let burst_capacity = config
        .burst
        .map_or(config.sustained.rate, |burst| burst.capacity);
    EffectiveRateLimit {
        replenish_per_sec,
        burst_capacity: f64::from(burst_capacity),
        cost: f64::from(config.cost),
        sustained_rate: config.sustained.rate,
        scope: config.scope,
        strategy: config.strategy,
    }
}

/// Request-identity inputs the counter-key scope value is derived from
/// (`cpt-cf-oagw-algo-effective-rate-limit`).
#[derive(Debug, Clone, Copy)]
pub struct RateLimitScopeContext<'a> {
    pub tenant_id: Uuid,
    pub subject_id: Uuid,
    pub client_ip: Option<&'a str>,
    pub route_id: Uuid,
}

/// Builds the counter key from resource kind, resource identifier, scope
/// name, and scope value (`cpt-cf-oagw-algo-effective-rate-limit`). The
/// resource kind and identifier lead the key so every counter for one
/// upstream shares a prefix.
#[must_use]
pub fn counter_key(
    resource_kind: &str,
    resource_id: Uuid,
    scope: RateLimitScope,
    ctx: &RateLimitScopeContext<'_>,
) -> String {
    let (scope_name, scope_value) = match scope {
        RateLimitScope::Global => ("global", "global".to_owned()),
        RateLimitScope::Tenant => ("tenant", ctx.tenant_id.to_string()),
        RateLimitScope::User => ("user", ctx.subject_id.to_string()),
        RateLimitScope::Ip => ("ip", ctx.client_ip.unwrap_or("unknown").to_owned()),
        RateLimitScope::Route => ("route", ctx.route_id.to_string()),
    };
    format!("{resource_kind}:{resource_id}:{scope_name}:{scope_value}")
}

/// The outcome of one admission attempt
/// (`cpt-cf-oagw-algo-token-bucket-admission`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AdmissionOutcome {
    pub admitted: bool,
    /// Whole tokens left in the bucket after the attempt.
    pub remaining: u32,
    /// Epoch second at which the bucket returns to full capacity.
    pub reset_epoch_secs: u64,
    /// Present only for a rejected attempt: whole seconds, at least one.
    pub retry_after_secs: Option<u64>,
}

/// Upper bound on the number of concurrently tracked rate-limit buckets,
/// mirroring `ResolvedConfigCache`'s L1 capacity (`resolve_cache.rs`) so the
/// crate stays internally consistent (CODE2-F-001, BUG2-F-002): unlike that
/// cache, a bucket map keyed in part on request-controlled input (e.g. the
/// `ip` scope's unvalidated `X-Forwarded-For` first hop) grew without bound
/// before this fix, letting a caller create unbounded entries with a random
/// value per request — a memory-exhaustion `DoS`.
const BUCKET_CAPACITY: usize = 10_000;

/// One per-counter bucket's mutable state, plus a monotonically increasing
/// logical-clock tick recording when it was last touched by [`RateLimiter::admit`].
#[derive(Debug)]
struct BucketState {
    tokens: f64,
    last_refill: Instant,
    /// Used only to pick the least-recently-used bucket for eviction once
    /// the map exceeds [`BUCKET_CAPACITY`] (CODE2-F-001, BUG2-F-002): every
    /// `admit()` call refreshes its own bucket's tick, so a bucket that is
    /// actively receiving requests — including one that is currently
    /// rejecting every one of them — can never be the map-wide minimum and
    /// is never the one reclaimed. Only a genuinely idle bucket is evicted,
    /// which is safe: a fresh bucket starts full, the same state an unseen
    /// key would have had anyway.
    last_used: u64,
}

/// Per-instance, in-process token-bucket admission
/// (`cpt-cf-oagw-dod-token-bucket-admission`, `cpt-cf-oagw-state-token-bucket`),
/// bounded at [`BUCKET_CAPACITY`] entries with least-recently-used eviction
/// (CODE2-F-001, BUG2-F-002).
#[derive(Debug, Default)]
pub struct RateLimiter {
    buckets: DashMap<String, BucketState>,
    /// Logical clock driving [`BucketState::last_used`]: a plain counter
    /// rather than wall-clock time, so eviction ordering never depends on
    /// system-clock resolution or skew.
    clock: AtomicU64,
}

impl RateLimiter {
    /// Builds an empty rate limiter with no buckets allocated.
    #[must_use]
    pub fn new() -> Self {
        Self {
            buckets: DashMap::new(),
            clock: AtomicU64::new(0),
        }
    }

    /// Admits or rejects one request against the bucket for `key`
    /// (`cpt-cf-oagw-algo-token-bucket-admission`). Creates a full bucket on
    /// first use, replenishes for the elapsed interval capped at burst
    /// capacity, and deducts `limit.cost` only on admission — a rejected
    /// request consumes no tokens at all. Evicts the least-recently-used
    /// bucket once a newly created bucket pushes the map past
    /// [`BUCKET_CAPACITY`] (CODE2-F-001, BUG2-F-002).
    // @cpt-begin:cpt-cf-oagw-dod-token-bucket-admission:p2:inst-token-bucket-admit-fn-01
    #[must_use]
    pub fn admit(&self, key: &str, limit: &EffectiveRateLimit) -> AdmissionOutcome {
        let now = Instant::now();
        let tick = self.clock.fetch_add(1, Ordering::Relaxed);
        let is_new_key = !self.buckets.contains_key(key);

        let outcome = {
            let mut bucket = self
                .buckets
                .entry(key.to_owned())
                .or_insert_with(|| BucketState {
                    tokens: limit.burst_capacity,
                    last_refill: now,
                    last_used: tick,
                });
            bucket.last_used = tick;

            let elapsed = now
                .saturating_duration_since(bucket.last_refill)
                .as_secs_f64();
            bucket.tokens =
                (bucket.tokens + elapsed * limit.replenish_per_sec).min(limit.burst_capacity);
            bucket.last_refill = now;

            let admitted = bucket.tokens >= limit.cost;
            if admitted {
                bucket.tokens -= limit.cost;
            }
            let remaining = bucket.tokens;
            let deficit = (limit.burst_capacity - remaining).max(0.0);
            let reset_epoch_secs =
                epoch_seconds_after(seconds_for(deficit, limit.replenish_per_sec));
            let retry_after_secs = (!admitted).then(|| {
                let missing = (limit.cost - remaining).max(0.0);
                seconds_for(missing, limit.replenish_per_sec).max(1)
            });

            AdmissionOutcome {
                admitted,
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                remaining: remaining.floor().max(0.0) as u32,
                reset_epoch_secs,
                retry_after_secs,
            }
        }; // The `entry` guard above is dropped here, releasing its shard
        // lock before the eviction scan below touches the same map.

        if is_new_key {
            self.evict_least_recently_used_if_over_capacity();
        }
        outcome
    }
    // @cpt-end:cpt-cf-oagw-dod-token-bucket-admission:p2:inst-token-bucket-admit-fn-01

    /// Evicts the single least-recently-used bucket once the map holds more
    /// than [`BUCKET_CAPACITY`] entries (CODE2-F-001, BUG2-F-002).
    fn evict_least_recently_used_if_over_capacity(&self) {
        if self.buckets.len() <= BUCKET_CAPACITY {
            return;
        }
        let oldest = self
            .buckets
            .iter()
            .min_by_key(|entry| entry.value().last_used)
            .map(|entry| entry.key().clone());
        if let Some(oldest_key) = oldest {
            self.buckets.remove(&oldest_key);
        }
    }

    /// Number of currently tracked buckets. Exposed for tests and
    /// diagnostics.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buckets.len()
    }

    /// `true` when no bucket is currently tracked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }
}

/// Whole seconds needed to replenish `amount` tokens at `rate` tokens per
/// second, rounded up. `rate <= 0` (never reachable for a validated
/// configuration) is treated as an immediate replenishment to avoid a
/// division by zero.
fn seconds_for(amount: f64, rate: f64) -> u64 {
    if amount <= 0.0 || rate <= 0.0 {
        return 0;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let secs = (amount / rate).ceil() as u64;
    secs
}

/// The current epoch second plus `seconds_ahead`.
fn epoch_seconds_after(seconds_ahead: u64) -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    now.as_secs().saturating_add(seconds_ahead)
}

/// The advisory `X-RateLimit-*`/`Retry-After` header values
/// (`cpt-cf-oagw-algo-rate-limit-headers`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateLimitHeaderValues {
    pub limit: u32,
    pub remaining: u32,
    pub reset_epoch_secs: u64,
    pub retry_after_secs: Option<u64>,
}

/// Builds the header values from one admission outcome
/// (`cpt-cf-oagw-algo-rate-limit-headers`). Accompanies admitted and
/// rejected requests alike.
#[must_use]
pub fn rate_limit_headers(
    outcome: &AdmissionOutcome,
    limit: &EffectiveRateLimit,
) -> RateLimitHeaderValues {
    RateLimitHeaderValues {
        limit: limit.sustained_rate,
        remaining: outcome.remaining,
        reset_epoch_secs: outcome.reset_epoch_secs,
        retry_after_secs: outcome.retry_after_secs,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BUCKET_CAPACITY, EffectiveRateLimit, RateLimitScopeContext, RateLimiter, counter_key,
        effective_rate_limit, rate_limit_headers,
    };
    use crate::domain::model::{
        Algorithm, Burst, RateLimitConfig, RateLimitScope, Sharing, Strategy, Sustained, Window,
    };
    use uuid::Uuid;

    fn config(rate: u32, window: Window, burst: Option<u32>, cost: u32) -> RateLimitConfig {
        RateLimitConfig {
            sharing: Sharing::Private,
            algorithm: Algorithm::TokenBucket,
            sustained: Sustained { rate, window },
            burst: burst.map(|capacity| Burst { capacity }),
            scope: RateLimitScope::Tenant,
            strategy: Strategy::Reject,
            cost,
        }
    }

    // @cpt-begin:cpt-cf-oagw-dod-effective-rate-limit:p2:inst-rate-limit-normalise-test-01
    #[test]
    fn burst_capacity_defaults_to_the_sustained_rate() {
        let effective = effective_rate_limit(&config(5, Window::Second, None, 1));
        assert!((effective.burst_capacity - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn a_per_minute_rate_normalises_to_a_per_second_replenishment() {
        let effective = effective_rate_limit(&config(60, Window::Minute, None, 1));
        assert!((effective.replenish_per_sec - 1.0).abs() < f64::EPSILON);
    }
    // @cpt-end:cpt-cf-oagw-dod-effective-rate-limit:p2:inst-rate-limit-normalise-test-01

    // @cpt-begin:cpt-cf-oagw-dod-effective-rate-limit:p2:inst-rate-limit-key-test-01
    #[test]
    fn counter_key_combines_resource_and_scope() {
        let upstream_id = Uuid::new_v4();
        let tenant_id = Uuid::new_v4();
        let ctx = RateLimitScopeContext {
            tenant_id,
            subject_id: Uuid::new_v4(),
            client_ip: None,
            route_id: Uuid::new_v4(),
        };
        let key = counter_key("upstream", upstream_id, RateLimitScope::Tenant, &ctx);
        assert_eq!(key, format!("upstream:{upstream_id}:tenant:{tenant_id}"));
    }

    #[test]
    fn tenant_scoped_keys_differ_across_tenants() {
        let upstream_id = Uuid::new_v4();
        let ctx_a = RateLimitScopeContext {
            tenant_id: Uuid::new_v4(),
            subject_id: Uuid::new_v4(),
            client_ip: None,
            route_id: Uuid::new_v4(),
        };
        let ctx_b = RateLimitScopeContext {
            tenant_id: Uuid::new_v4(),
            ..ctx_a
        };
        assert_ne!(
            counter_key("upstream", upstream_id, RateLimitScope::Tenant, &ctx_a),
            counter_key("upstream", upstream_id, RateLimitScope::Tenant, &ctx_b)
        );
    }
    // @cpt-end:cpt-cf-oagw-dod-effective-rate-limit:p2:inst-rate-limit-key-test-01

    // @cpt-begin:cpt-cf-oagw-dod-token-bucket-admission:p2:inst-token-bucket-admit-test-01
    #[test]
    fn a_bucket_of_capacity_one_admits_once_then_rejects() {
        let limiter = RateLimiter::new();
        let limit = effective_rate_limit(&config(1, Window::Second, Some(1), 1));

        let first = limiter.admit("k", &limit);
        assert!(first.admitted);
        let second = limiter.admit("k", &limit);
        assert!(!second.admitted);
        assert!(second.retry_after_secs.is_some_and(|secs| secs >= 1));
    }
    // @cpt-end:cpt-cf-oagw-dod-token-bucket-admission:p2:inst-token-bucket-admit-test-01

    #[test]
    fn a_rejected_request_consumes_no_tokens() {
        let limiter = RateLimiter::new();
        let limit = effective_rate_limit(&config(10, Window::Second, Some(10), 10));

        let first = limiter.admit("k2", &limit);
        assert!(first.admitted);
        assert_eq!(first.remaining, 0);
        let second = limiter.admit("k2", &limit);
        assert!(!second.admitted);
        assert_eq!(second.remaining, 0);
    }

    #[test]
    fn distinct_keys_have_independent_buckets() {
        let limiter = RateLimiter::new();
        let limit = effective_rate_limit(&config(1, Window::Second, Some(1), 1));
        assert!(limiter.admit("tenant-a", &limit).admitted);
        assert!(limiter.admit("tenant-b", &limit).admitted);
    }

    // CODE2-F-001 / BUG2-F-002 regression: the bucket map must stay bounded
    // under an unbounded number of distinct counter keys (e.g. an
    // unvalidated `ip`-scope value supplied fresh on every request), and
    // eviction must never reset a bucket that is actively being throttled.

    #[test]
    fn the_bucket_map_stops_growing_past_its_capacity_under_many_distinct_keys() {
        let limiter = RateLimiter::new();
        let limit = effective_rate_limit(&config(1, Window::Second, Some(1), 1));

        for i in 0..(BUCKET_CAPACITY + 50) {
            let _ignored_outcome = limiter.admit(&format!("distinct-key-{i}"), &limit);
        }

        assert!(
            limiter.len() <= BUCKET_CAPACITY,
            "the map must never grow past its capacity, got {}",
            limiter.len()
        );
    }

    #[test]
    fn eviction_never_resets_a_bucket_that_is_actively_being_throttled() {
        let limiter = RateLimiter::new();
        let limit = effective_rate_limit(&config(1, Window::Second, Some(1), 1));

        // Exhaust one "hot" key up front, then keep hitting it once per
        // filler key inserted, so its own recency tick is refreshed on
        // every iteration and it is never the map-wide least-recently-used
        // entry.
        let hot_key = "hot-tenant";
        assert!(limiter.admit(hot_key, &limit).admitted);
        assert!(
            !limiter.admit(hot_key, &limit).admitted,
            "the hot key's single-token bucket must already be exhausted"
        );

        for i in 0..(BUCKET_CAPACITY + 50) {
            let _ignored_outcome = limiter.admit(&format!("filler-key-{i}"), &limit);
            let outcome = limiter.admit(hot_key, &limit);
            assert!(
                !outcome.admitted,
                "an actively-throttled bucket must never be evicted back to a fresh, full state"
            );
        }
        assert!(limiter.len() <= BUCKET_CAPACITY);
    }

    #[test]
    fn a_cost_exceeding_capacity_is_always_rejected() {
        let limiter = RateLimiter::new();
        let limit = EffectiveRateLimit {
            replenish_per_sec: 1.0,
            burst_capacity: 1.0,
            cost: 5.0,
            sustained_rate: 1,
            scope: RateLimitScope::Tenant,
            strategy: Strategy::Reject,
        };
        assert!(!limiter.admit("over-cost", &limit).admitted);
    }

    // @cpt-begin:cpt-cf-oagw-dod-rate-limit-response:p2:inst-rate-limit-headers-test-01
    #[test]
    fn header_values_mirror_the_admission_outcome() {
        let limiter = RateLimiter::new();
        let limit = effective_rate_limit(&config(1, Window::Second, Some(1), 1));
        let _ignored_outcome = limiter.admit("h", &limit);
        let outcome = limiter.admit("h", &limit);
        let headers = rate_limit_headers(&outcome, &limit);
        assert_eq!(headers.limit, 1);
        assert_eq!(headers.remaining, 0);
        assert!(headers.retry_after_secs.is_some_and(|secs| secs >= 1));
    }
    // @cpt-end:cpt-cf-oagw-dod-rate-limit-response:p2:inst-rate-limit-headers-test-01

    #[test]
    fn queue_and_degrade_strategies_are_read_but_never_change_admission() {
        let limiter = RateLimiter::new();
        let mut limit = effective_rate_limit(&config(1, Window::Second, Some(1), 1));
        limit.strategy = Strategy::Queue;
        assert!(limiter.admit("queue-key", &limit).admitted);
        assert!(!limiter.admit("queue-key", &limit).admitted);
    }
}
