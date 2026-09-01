//! In-memory rate-limit state (token buckets) and the per-upstream circuit
//! breaker.
//!
//! Buckets are keyed by the owning resource (upstream or route) plus the
//! effective limit's `scope` dimension (ADR-0003's
//! `oagw:ratelimit:{resource_type}:{resource_id}:{scope}:{scope_id}`), so two
//! upstreams — or two tenants behind the same upstream — never share a counter.
//! The `{window}` segment of the ADR key only exists for Redis' fixed-window
//! counters; the token bucket refills continuously and needs no window segment.
//! Per-instance counters are an accepted MVP limitation (ADR-0003 / ADR-0006).
//!
//! The registry is bounded: at most [`MAX_BUCKETS`] buckets stay alive, the
//! oldest-inserted one is evicted first, so a key space driven by client
//! addresses or tenant ids cannot grow without limit.

use std::collections::VecDeque;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use dashmap::DashMap;
use parking_lot::Mutex;

/// Decision returned by a rate-limit check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateDecision {
    /// Tokens available; request admitted.
    Allowed {
        /// Configured limit.
        limit: u32,
        /// Tokens left after this request.
        remaining: u32,
        /// Seconds until the bucket is fully replenished.
        reset_seconds: u64,
    },
    /// Bucket exhausted; request rejected.
    Rejected {
        /// Configured limit.
        limit: u32,
        /// Seconds the client should wait before retrying.
        retry_after_seconds: u64,
    },
}

impl RateDecision {
    /// `true` when the request was admitted.
    #[must_use]
    pub fn is_allowed(&self) -> bool {
        matches!(self, RateDecision::Allowed { .. })
    }

    /// The configured limit, for both outcomes.
    #[must_use]
    pub fn limit(&self) -> u32 {
        match *self {
            RateDecision::Allowed { limit, .. } | RateDecision::Rejected { limit, .. } => limit,
        }
    }

    /// Tokens left after an admitted request (0 when rejected).
    #[must_use]
    pub fn remaining(&self) -> u32 {
        match *self {
            RateDecision::Allowed { remaining, .. } => remaining,
            RateDecision::Rejected { .. } => 0,
        }
    }

    /// `Retry-After` hint in seconds (0 when admitted or no reset known).
    #[must_use]
    pub fn retry_after(&self) -> u64 {
        match *self {
            RateDecision::Allowed { .. } => 0,
            RateDecision::Rejected {
                retry_after_seconds,
                ..
            } => retry_after_seconds,
        }
    }

    /// Seconds until the bucket is fully replenished (0 when rejected).
    #[must_use]
    pub fn reset_seconds(&self) -> u64 {
        match *self {
            RateDecision::Allowed { reset_seconds, .. } => reset_seconds,
            RateDecision::Rejected { .. } => 0,
        }
    }
}

/// A token bucket: `capacity` tokens, refilled at `refill_per_second`.
#[derive(Debug)]
struct Bucket {
    /// Tokens currently in the bucket.
    tokens: f64,
    /// Maximum tokens.
    capacity: u32,
    /// Refill rate in tokens per second.
    refill_per_second: f64,
    /// Last refill instant (monotonic millis).
    last_refill_ms: u64,
}

impl Bucket {
    fn new(capacity: u32, refill_per_second: f64, now_ms: u64) -> Self {
        Self {
            tokens: f64::from(capacity),
            capacity,
            refill_per_second,
            last_refill_ms: now_ms,
        }
    }

    fn refill(&mut self, now_ms: u64) {
        let elapsed_ms = now_ms.saturating_sub(self.last_refill_ms);
        if elapsed_ms == 0 {
            return;
        }
        let elapsed_secs = seconds_of_millis(elapsed_ms);
        self.tokens =
            (self.tokens + elapsed_secs * self.refill_per_second).min(f64::from(self.capacity));
        self.last_refill_ms = now_ms;
    }
}

/// Seconds covered by a millisecond span, keeping the sub-second part.
///
/// The whole-second part is capped at `u32::MAX` seconds (136 years of bucket
/// idleness), which is far beyond any cooldown or refill horizon.
fn seconds_of_millis(millis: u64) -> f64 {
    let whole = u32::try_from(millis / 1_000).unwrap_or(u32::MAX);
    let rest = u32::try_from(millis % 1_000).unwrap_or(0);
    f64::from(whole) + f64::from(rest) / 1_000.0
}

/// Process-wide monotonic epoch, captured on first use.
static MONOTONIC_EPOCH: OnceLock<std::time::Instant> = OnceLock::new();

/// Monotonic milliseconds since this process first read the clock.
///
/// [`std::time::Instant`] never goes backwards and is unaffected by wall-clock
/// adjustments, so bucket refill and breaker cooldowns cannot be skewed by an
/// NTP step or by a leap second. The value is only meaningful inside this
/// process: it is not the wall clock and is never persisted.
fn now_ms() -> u64 {
    let elapsed = MONOTONIC_EPOCH
        .get_or_init(std::time::Instant::now)
        .elapsed();
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

/// Upper bound on live buckets per registry.
///
/// A key space driven by client addresses (`scope: ip`) or by tenant ids is
/// attacker-influenceable, so the registry is capped: once the bound is
/// reached, the oldest-inserted bucket is evicted before a new one is created.
/// The bound trades a worst-case 100k × ~64 bytes of counter state for the
/// guarantee that the Data Plane's memory footprint is flat.
pub const MAX_BUCKETS: usize = 100_000;

/// Registry of token buckets, per owning resource and rate-limit scope.
#[derive(Default)]
pub struct RateLimiterRegistry {
    buckets: DashMap<String, Mutex<Bucket>>,
    /// Insertion order of the live buckets, oldest first (eviction ring).
    order: Mutex<VecDeque<String>>,
}

impl RateLimiterRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of live buckets (observability / test hook).
    #[must_use]
    pub fn bucket_count(&self) -> usize {
        self.buckets.len()
    }

    /// Builds the counter key for a rate-limit scope.
    ///
    /// The key follows ADR-0003's
    /// `{resource_type}:{resource_id}:{scope}:{scope_id}` layout. The owning
    /// resource is the route for [`crate::domain::model::RateScope::Route`] and
    /// the upstream for every other scope, because the effective limit of a
    /// non-`Route` scope applies to the upstream as a whole. The tenant id is
    /// always part of the key for the `global`, `ip` and `route` scopes, so two
    /// tenants behind one upstream can never share a counter.
    ///
    /// The key is deliberately free of credential material: it is composed of
    /// the resource ids, the scope dimension and the scope value.
    #[must_use]
    pub fn scope_key(
        scope: crate::domain::model::RateScope,
        upstream_id: uuid::Uuid,
        route_id: Option<uuid::Uuid>,
        tenant_id: uuid::Uuid,
        subject_id: uuid::Uuid,
        client_ip: Option<&str>,
    ) -> String {
        use crate::domain::model::RateScope;
        let (resource_type, resource_id) = match (scope, route_id) {
            (RateScope::Route, Some(route_id)) => ("route", route_id.to_string()),
            _ => ("upstream", upstream_id.to_string()),
        };
        let scope_id = match scope {
            RateScope::Global | RateScope::Tenant => tenant_id.to_string(),
            RateScope::User => format!("{tenant_id}:{subject_id}"),
            RateScope::Ip => format!("{tenant_id}:{}", client_ip.unwrap_or("unknown")),
            RateScope::Route => tenant_id.to_string(),
        };
        format!(
            "{resource_type}:{resource_id}:{}:{scope_id}",
            scope_label(scope)
        )
    }

    /// Consumes `cost` tokens from the bucket identified by `key`.
    ///
    /// The winning limit is applied unconditionally — `capacity` and the refill
    /// rate are overwritten and the stored tokens are clamped to the new
    /// capacity — so a tightened configuration takes effect on the next request
    /// instead of being masked by the capacity a looser one left behind.
    ///
    /// # Panics
    ///
    /// Never: the bucket mutex is only held for the refill/consume arithmetic.
    pub fn check(&self, key: &str, limit: &crate::domain::model::RateLimitConfig) -> RateDecision {
        let now = now_ms();
        let capacity = limit.capacity();
        let refill_per_second = limit.refill_per_second();
        let existed = self.buckets.contains_key(key);
        let decision = {
            let entry = self
                .buckets
                .entry(key.to_owned())
                .or_insert_with(|| Mutex::new(Bucket::new(capacity, refill_per_second, now)));
            let mut bucket = entry.value().lock();
            bucket.capacity = capacity;
            bucket.refill_per_second = refill_per_second;
            if bucket.tokens > f64::from(capacity) {
                bucket.tokens = f64::from(capacity);
            }
            bucket.refill(now);

            let cost = f64::from(limit.cost.max(1));
            if bucket.tokens >= cost {
                bucket.tokens -= cost;
                let remaining = clamp_tokens(bucket.tokens, capacity);
                let reset_seconds = if refill_per_second <= 0.0 {
                    0
                } else {
                    ((f64::from(bucket.capacity) - bucket.tokens) / refill_per_second).ceil() as u64
                };
                RateDecision::Allowed {
                    limit: capacity,
                    remaining,
                    reset_seconds,
                }
            } else {
                RateDecision::Rejected {
                    limit: capacity,
                    retry_after_seconds: limit.retry_after_seconds().max(1),
                }
            }
        };
        if !existed {
            self.track(key);
        }
        decision
    }

    /// Registers a freshly created bucket and enforces the registry bound.
    fn track(&self, key: &str) {
        let mut order = self.order.lock();
        order.push_back(key.to_owned());
        if self.buckets.len() <= MAX_BUCKETS {
            return;
        }
        // Drop the oldest buckets until the registry fits again. A key can
        // appear twice in the ring after a lost race, which only costs one
        // extra eviction candidate.
        while self.buckets.len() > MAX_BUCKETS {
            let Some(oldest) = order.pop_front() else {
                break;
            };
            self.buckets.remove(&oldest);
        }
    }

    /// Current bucket fill ratio (0.0 - 1.0) for the usage gauge.
    #[must_use]
    pub fn usage_ratio(&self, key: &str, capacity: u32) -> f64 {
        if capacity == 0 {
            return 0.0;
        }
        self.buckets.get(key).map_or(0.0, |entry| {
            let bucket = entry.value().lock();
            1.0 - (bucket.tokens / f64::from(capacity)).clamp(0.0, 1.0)
        })
    }

    /// Forgets a bucket (cache hygiene after configuration deletion).
    pub fn forget(&self, key: &str) {
        self.buckets.remove(key);
    }
}

fn clamp_tokens(tokens: f64, capacity: u32) -> u32 {
    let clamped = tokens.clamp(0.0, f64::from(capacity));
    clamped as u32
}

/// Lower-cased, ADR-0003 scope token used in the counter key.
#[must_use]
pub fn scope_label(scope: crate::domain::model::RateScope) -> &'static str {
    use crate::domain::model::RateScope;
    match scope {
        RateScope::Global => "global",
        RateScope::Tenant => "tenant",
        RateScope::User => "user",
        RateScope::Ip => "ip",
        RateScope::Route => "route",
    }
}

/// Circuit breaker states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    /// Normal operation.
    Closed,
    /// Requests rejected until the cooldown elapses.
    Open,
    /// A probe request is allowed through.
    HalfOpen,
}

/// Per-upstream circuit breaker.
///
/// Thresholds are module constants rather than configuration keys: the
/// breaker is core Data Plane functionality and DESIGN §4.7 lists
/// configuration as future work.
///
/// In [`CircuitState::HalfOpen`] exactly one request at a time is admitted as a
/// probe ([`CircuitBreaker::begin_probe`]); every other request is rejected
/// until the probe succeeds (`record_success` → closed) or fails
/// (`record_failure`, which needs `failure_threshold` consecutive failures to
/// re-open the breaker).
#[derive(Debug)]
pub struct CircuitBreaker {
    state: Mutex<BreakerState>,
    /// Number of half-open probes currently admitted (0 or 1).
    in_flight_probes: AtomicUsize,
    /// Consecutive failures before the breaker opens.
    failure_threshold: u32,
    /// Cooldown before half-open probing.
    cooldown: std::time::Duration,
}

#[derive(Debug, Clone, Copy)]
struct BreakerState {
    state: CircuitState,
    consecutive_failures: u32,
    opened_at_ms: u64,
}

impl CircuitBreaker {
    /// Creates a breaker with the given thresholds.
    #[must_use]
    pub fn new(failure_threshold: u32, cooldown: std::time::Duration) -> Self {
        Self {
            state: Mutex::new(BreakerState {
                state: CircuitState::Closed,
                consecutive_failures: 0,
                opened_at_ms: 0,
            }),
            in_flight_probes: AtomicUsize::new(0),
            failure_threshold,
            cooldown,
        }
    }

    /// Default breaker: 5 consecutive failures, 30 s cooldown.
    #[must_use]
    pub fn default_breaker() -> Self {
        Self::new(5, std::time::Duration::from_secs(30))
    }

    /// Current state, transitioning `Open` → `HalfOpen` after the cooldown.
    #[must_use]
    pub fn state(&self) -> CircuitState {
        let mut state = self.state.lock();
        if state.state == CircuitState::Open {
            let cooldown_ms = u64::try_from(self.cooldown.as_millis()).unwrap_or(u64::MAX);
            if now_ms().saturating_sub(state.opened_at_ms) >= cooldown_ms {
                state.state = CircuitState::HalfOpen;
                state.consecutive_failures = 0;
            }
        }
        state.state
    }

    /// Admits a single probe request through a half-open breaker.
    ///
    /// Returns `false` while another probe is still in flight; the caller must
    /// then reject the request with `CircuitBreakerOpen`. A successful caller
    /// owns the slot until it calls [`CircuitBreaker::end_probe`].
    pub fn begin_probe(&self) -> bool {
        self.in_flight_probes
            .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    /// Releases the half-open probe slot acquired with [`begin_probe`].
    pub fn end_probe(&self) {
        let _ = self
            .in_flight_probes
            .compare_exchange(1, 0, Ordering::SeqCst, Ordering::SeqCst);
    }

    /// Seconds until half-open probing resumes (0 when not open).
    #[must_use]
    pub fn retry_after_seconds(&self) -> u64 {
        let state = self.state.lock();
        if state.state != CircuitState::Open {
            return 0;
        }
        let elapsed = now_ms().saturating_sub(state.opened_at_ms);
        let cooldown_ms = u64::try_from(self.cooldown.as_millis()).unwrap_or(u64::MAX);
        cooldown_ms.saturating_sub(elapsed).div_ceil(1_000)
    }

    /// Records a successful upstream call.
    pub fn record_success(&self) -> Option<(CircuitState, CircuitState)> {
        let mut state = self.state.lock();
        let previous = state.state;
        state.consecutive_failures = 0;
        state.state = CircuitState::Closed;
        (previous != CircuitState::Closed).then_some((previous, CircuitState::Closed))
    }

    /// Records a failed upstream call.
    ///
    /// # Panics
    ///
    /// Never: the lock is only held for integer arithmetic.
    pub fn record_failure(&self) -> Option<(CircuitState, CircuitState)> {
        let mut state = self.state.lock();
        let previous = state.state;
        state.consecutive_failures = state.consecutive_failures.saturating_add(1);
        if state.consecutive_failures >= self.failure_threshold {
            state.state = CircuitState::Open;
            state.opened_at_ms = now_ms();
            (previous != CircuitState::Open).then_some((previous, CircuitState::Open))
        } else {
            None
        }
    }
}

/// Monotonic-seconds helper for tests.
#[must_use]
pub fn elapsed_millis(since_ms: u64) -> u64 {
    now_ms().saturating_sub(since_ms)
}

#[cfg(test)]
#[path = "ratelimit_tests.rs"]
mod tests;
