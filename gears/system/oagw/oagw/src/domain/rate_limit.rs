//! Rate limiting (ADR-0003): token bucket with dual-rate configuration,
//! optional sliding window, hierarchical inheritance and the in-memory
//! limiter registry the data plane consults.
//!
//! ## Reference algorithm
//!
//! The token bucket follows the ADR-0003 reference implementation exactly:
//!
//! ```text
//! tokens = min(tokens + elapsed * refill_rate, capacity)
//! acquire(cost) when tokens >= cost, then tokens -= cost
//! ```
//!
//! `refill_rate` is derived from the dual-rate configuration as
//! `sustained.rate / window` and `capacity` defaults to `sustained.rate` when
//! `burst.capacity` is omitted, so a bucket starts full and absorbs a burst up
//! to its capacity before throttling to the sustained rate.
//!
//! The clock is passed in by the caller ([`Instant`]) instead of being read
//! with [`std::time::Instant::now`] inside the bucket: every decision is
//! therefore reproducible in tests and the registry stays free of hidden
//! global state.
//!
//! ## Inheritance
//!
//! [`resolve_effective_rate_limit`] walks an ancestor → descendant chain of
//! [`RateLimitConfig`] and applies the ADR-0003 inheritance table, keyed on
//! the *outer* entry's `sharing` mode:
//!
//! | Outer sharing | Inner specifies | Effective |
//! |---|---|---|
//! | `private` | any | inner only |
//! | `inherit` | none | outer's limit |
//! | `inherit` | own | `min(outer, inner)` |
//! | `enforce` | any | `min(outer, inner)` |
//!
//! "Specifies none" is expressed by the entry being absent from the chain
//! (`sustained.rate` is required by the schema, so a `rate_limit` object
//! always carries a rate). Rates are compared as tokens per second, so entries
//! declared with different windows are still ordered correctly; the window of
//! the winning entry is kept.
//!
//! ## Strategies
//!
//! * `reject` — the ADR behaviour: no tokens means `429` plus `Retry-After`.
//! * `queue` — **deviation, documented.** ADR-0003 names `queue` as an enum
//!   value but defines no queue depth or wait semantics. Here a request whose
//!   tokens are not yet available is *reserved* when the projected wait is at
//!   most [`QUEUE_MAX_WAIT`], by debiting the bucket (which may go negative —
//!   a credit for work already promised). The decision carries
//!   [`RateLimitDecision::queue_wait`] and the data plane awaits it before
//!   forwarding; past the bound the request is rejected like `reject`.
//!   A sliding window is a log of past hits rather than a refillable balance,
//!   so a reservation inside the bound can still fail to deliver: the request
//!   is then answered `429`, always with `Retry-After` (ADR-0003), and nothing
//!   is debited (a log cannot go into credit), so no reservation is captured.
//!   A granted reservation is refunded through
//!   [`RateLimiterRegistry::reservation`] → [`RateLimiterRegistry::release`]
//!   when the request fails afterwards.
//! * `degrade` — **deviation, documented.** The request is always served; when
//!   the bucket could not cover the cost the decision is flagged
//!   ([`RateLimitDecision::degraded`]) and the response carries the
//!   `X-OAGW-Degraded` marker header. Only the tokens actually available are
//!   debited.
//!
//! ## Bucket lifetime
//!
//! A bucket is created on demand and lives until it is dropped, so the registry
//! needs an explicit bound: its key space is
//! `upstream × scope × client`, and the client half of that is not under the
//! operator's control (an `ip`-scoped limit over the public internet would grow
//! a bucket per address). [`RateLimiterRegistry`] therefore holds at most
//! [`MAX_BUCKETS`] buckets and evicts the least recently used eighth once the
//! bound is reached — one scan per batch, amortised over a thousand
//! insertions, and never more work than the check itself. A bucket is dropped
//! with its upstream as well: the store calls
//! [`RateLimiterRegistry::clear_upstream`] when an upstream is deleted, so a
//! recreated upstream of the same alias starts from an empty budget instead of
//! inheriting the deleted one's.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use axum::http::{HeaderName, HeaderValue};
use dashmap::DashMap;
use uuid::Uuid;

use crate::domain::error::OagwError;
use crate::domain::model::{
    BurstConfig, RateLimitAlgorithm, RateLimitConfig, RateLimitScope, RateLimitStrategy,
    RateLimitWindow, SharingMode,
};

/// Longest wait a `queue` strategy request is allowed to spend waiting for
/// tokens before it is rejected (see the module docs for the deviation note).
pub const QUEUE_MAX_WAIT: Duration = Duration::from_secs(1);

/// Upper bound of live buckets in a [`RateLimiterRegistry`].
///
/// One bucket per `(upstream, scope, client)` key that has been seen, so an
/// `ip`-scoped limit over the public internet is the only way to reach this —
/// and 8192 addresses is far below what one process can afford.
pub const MAX_BUCKETS: usize = 8192;

/// Fraction of [`MAX_BUCKETS`] evicted at once, least recently used first.
const EVICTION_BATCH: usize = MAX_BUCKETS / 8;

/// Marker header emitted when a `degrade` strategy served a throttled request.
pub const DEGRADED_HEADER: &str = "x-oagw-degraded";

/// Value of [`DEGRADED_HEADER`].
pub const DEGRADED_HEADER_VALUE: &str = "rate-limit";

/// `X-RateLimit-Limit` header name.
pub const RATE_LIMIT_HEADER: &str = "x-ratelimit-limit";
/// `X-RateLimit-Remaining` header name.
pub const RATE_LIMIT_REMAINING_HEADER: &str = "x-ratelimit-remaining";
/// `X-RateLimit-Reset` header name (absolute epoch seconds).
pub const RATE_LIMIT_RESET_HEADER: &str = "x-ratelimit-reset";
/// `Retry-After` header name.
pub const RETRY_AFTER_HEADER: &str = "retry-after";

/// GTS error type id of the `429` problem document (ADR-0003).
pub const RATE_LIMIT_EXCEEDED_TYPE: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1";

/// Duration of one rate-limit window unit.
#[must_use]
pub fn window_duration(window: RateLimitWindow) -> Duration {
    match window {
        RateLimitWindow::Second => Duration::from_secs(1),
        RateLimitWindow::Minute => Duration::from_secs(60),
        RateLimitWindow::Hour => Duration::from_secs(60 * 60),
        RateLimitWindow::Day => Duration::from_secs(60 * 60 * 24),
    }
}

// ---------------------------------------------------------------------------
// Effective configuration
// ---------------------------------------------------------------------------

/// Fully resolved rate-limit configuration: what a limiter actually enforces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveRateLimit {
    /// Enforcement algorithm.
    pub algorithm: RateLimitAlgorithm,
    /// Sustained tokens replenished per [`Self::window`].
    pub sustained_rate: u64,
    /// Window unit of the sustained rate.
    pub window: RateLimitWindow,
    /// Maximum burst (bucket capacity); defaults to `sustained_rate`.
    pub capacity: u64,
    /// Counter scope.
    pub scope: RateLimitScope,
    /// Over-limit behaviour.
    pub strategy: RateLimitStrategy,
    /// Whether `X-RateLimit-*` headers are emitted.
    pub response_headers: bool,
    /// Tokens consumed per request.
    pub cost: u64,
}

impl EffectiveRateLimit {
    /// Projects one configuration entry onto the effective shape.
    ///
    /// `burst.capacity` falls back to `sustained.rate` (ADR-0003) and `cost`
    /// falls back to `1`.
    #[must_use]
    pub fn from_config(config: &RateLimitConfig) -> Self {
        Self {
            algorithm: config.algorithm,
            sustained_rate: config.sustained.rate,
            window: config.sustained.window,
            capacity: Self::capacity_of(config),
            scope: config.scope,
            strategy: config.strategy,
            response_headers: config.response_headers,
            cost: config.cost.max(1),
        }
    }

    fn capacity_of(config: &RateLimitConfig) -> u64 {
        config
            .burst
            .as_ref()
            .and_then(|burst: &BurstConfig| burst.capacity)
            .unwrap_or(config.sustained.rate)
            .max(1)
    }

    /// Window length of this limit.
    #[must_use]
    pub fn window_duration(&self) -> Duration {
        window_duration(self.window)
    }

    /// Sustained replenishment rate in tokens per second.
    ///
    /// [`Duration::as_secs_f64`] cannot produce a negative value here, so the
    /// division is exact for the window units the schema allows.
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        reason = "rate magnitudes fit f64 exactly for the window units the schema allows"
    )]
    pub fn refill_rate(&self) -> f64 {
        let window_secs = self.window_duration().as_secs_f64();
        if window_secs <= 0.0 {
            return 0.0;
        }
        self.sustained_rate as f64 / window_secs
    }

    /// Bucket capacity as `f64`.
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        reason = "bucket capacities are bounded by configuration integers well below f64 precision"
    )]
    pub fn capacity_f64(&self) -> f64 {
        self.capacity as f64
    }

    /// Cost of one request as `f64`.
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        reason = "request costs are small configuration integers"
    )]
    pub fn cost_f64(&self) -> f64 {
        self.cost as f64
    }
}

/// Resolves the effective limit over an ancestor → descendant chain of
/// rate-limit configurations (ADR-0003 inheritance table).
///
/// `chain` must be ordered outermost (most ancestral / upstream) first and
/// innermost (route) last. Returns `None` when no entry contributes a limit.
#[must_use]
pub fn resolve_effective_rate_limit(chain: &[&RateLimitConfig]) -> Option<EffectiveRateLimit> {
    let mut effective: Option<EffectiveRateLimit> = None;
    for (index, config) in chain.iter().enumerate() {
        let own = EffectiveRateLimit::from_config(config);
        effective = Some(match effective {
            // The outermost entry always contributes its own limit.
            None => own,
            Some(parent) => match chain[index - 1].sharing {
                SharingMode::Private => own,
                SharingMode::Inherit | SharingMode::Enforce => merge_restrictive(&parent, &own),
            },
        });
    }
    effective
}

/// `min(parent, child)` over the sustained rate (normalised to tokens per
/// second) and the burst capacity. The window, scope, strategy, cost and
/// header switches of the *winning* entry are kept, so the most restrictive
/// configuration decides how the counter is shaped.
#[allow(
    clippy::cast_precision_loss,
    reason = "rates are compared as tokens per second; magnitudes fit f64"
)]
fn merge_restrictive(
    parent: &EffectiveRateLimit,
    child: &EffectiveRateLimit,
) -> EffectiveRateLimit {
    let parent_rate = parent.sustained_rate as f64 / parent.window_duration().as_secs_f64();
    let child_rate = child.sustained_rate as f64 / child.window_duration().as_secs_f64();
    if child_rate <= parent_rate {
        child.clone()
    } else {
        parent.clone()
    }
}

// ---------------------------------------------------------------------------
// Token bucket (ADR-0003 reference algorithm)
// ---------------------------------------------------------------------------

/// Token bucket: sustained replenishment with burst capacity.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TokenBucket {
    /// Tokens currently available.
    pub tokens: f64,
    /// Maximum burst size.
    pub capacity: f64,
    /// Tokens replenished per second.
    pub refill_rate: f64,
    /// Instant the bucket was last refilled.
    pub last_refill: Instant,
}

impl TokenBucket {
    /// Creates a full bucket.
    #[must_use]
    pub fn new(capacity: f64, refill_rate: f64, now: Instant) -> Self {
        Self {
            tokens: capacity,
            capacity,
            refill_rate,
            last_refill: now,
        }
    }

    /// Replenishes the bucket: `tokens = min(tokens + elapsed * rate, capacity)`.
    ///
    /// A clock that runs backwards (never on a monotonic [`Instant`], but
    /// cheap to defend against) is treated as no elapsed time.
    pub fn refill(&mut self, now: Instant) {
        let elapsed = now
            .checked_duration_since(self.last_refill)
            .unwrap_or_default();
        self.tokens = (self.tokens + elapsed.as_secs_f64() * self.refill_rate).min(self.capacity);
        self.last_refill = now;
    }

    /// Acquires `cost` tokens, refilling first.
    ///
    /// Returns `true` and debits the bucket when at least `cost` tokens are
    /// available, `false` and leaves the bucket untouched otherwise.
    pub fn try_acquire(&mut self, cost: f64, now: Instant) -> bool {
        self.refill(now);
        if self.tokens >= cost {
            self.tokens -= cost;
            true
        } else {
            false
        }
    }

    /// Acquires `cost` tokens, debiting the bucket even below zero (a credit
    /// for work already promised). Used by the `queue` strategy.
    pub fn acquire_credit(&mut self, cost: f64, now: Instant) {
        self.refill(now);
        self.tokens -= cost;
        if self.tokens < -self.capacity {
            self.tokens = -self.capacity;
        }
    }

    /// Debits as many tokens as are available, never below zero. Used by the
    /// `degrade` strategy.
    pub fn acquire_partial(&mut self, cost: f64, now: Instant) -> bool {
        self.refill(now);
        if self.tokens >= cost {
            self.tokens -= cost;
            true
        } else {
            self.tokens = 0.0;
            false
        }
    }

    /// Refunds `cost` tokens a granted `queue` reservation debited, so a
    /// request that fails after the debit cannot spend the budget twice.
    ///
    /// The bucket is refilled first and the debit is then undone, which
    /// restores exactly the balance the bucket would carry had it never been
    /// debited — the refill the wait granted is not lost, and the balance is
    /// clamped to the capacity so a late refund cannot mint tokens.
    pub fn release(&mut self, cost: f64, now: Instant) {
        self.refill(now);
        self.tokens = (self.tokens + cost).min(self.capacity);
    }

    /// Duration until the bucket holds at least `target` tokens.
    ///
    /// [`Duration::MAX`] when the bucket never replenishes.
    #[must_use]
    pub fn time_to_tokens(&self, target: f64, now: Instant) -> Duration {
        if self.refill_rate <= 0.0 {
            return Duration::MAX;
        }
        let elapsed = now
            .checked_duration_since(self.last_refill)
            .unwrap_or_default();
        let projected = (self.tokens + elapsed.as_secs_f64() * self.refill_rate).min(self.capacity);
        if projected >= target {
            return Duration::ZERO;
        }
        let deficit = target - projected;
        Duration::from_secs_f64(deficit / self.refill_rate)
    }

    /// Tokens available at `now` without mutating the bucket.
    #[must_use]
    pub fn tokens_at(&self, now: Instant) -> f64 {
        let elapsed = now
            .checked_duration_since(self.last_refill)
            .unwrap_or_default();
        (self.tokens + elapsed.as_secs_f64() * self.refill_rate).min(self.capacity)
    }
}

// ---------------------------------------------------------------------------
// Sliding window
// ---------------------------------------------------------------------------

/// Sliding-window counter: at most `capacity` tokens inside any `window`.
///
/// The window is a log of `(instant, cost)` hits, pruned on every check, so
/// memory is bounded by `capacity` entries per active key.
#[derive(Debug, Clone, PartialEq)]
pub struct SlidingWindow {
    capacity: f64,
    window: Duration,
    hits: VecDeque<(Instant, f64)>,
    consumed: f64,
}

impl SlidingWindow {
    /// Creates an empty window.
    #[must_use]
    pub fn new(capacity: f64, window: Duration) -> Self {
        Self {
            capacity,
            window,
            hits: VecDeque::new(),
            consumed: 0.0,
        }
    }

    /// Drops hits that left the window.
    fn prune(&mut self, now: Instant) {
        while let Some((at, cost)) = self.hits.front() {
            if now.checked_duration_since(*at).unwrap_or_default() < self.window {
                break;
            }
            self.consumed -= cost;
            self.hits.pop_front();
        }
    }

    /// Acquires `cost` tokens.
    pub fn try_acquire(&mut self, cost: f64, now: Instant) -> bool {
        self.prune(now);
        if self.consumed + cost > self.capacity {
            return false;
        }
        self.hits.push_back((now, cost));
        self.consumed += cost;
        true
    }

    /// Refunds `cost` tokens a granted `queue` reservation debited, so a
    /// request that fails after the debit cannot spend the budget twice.
    ///
    /// The window is a log of hits rather than a balance, so the refund drops
    /// the *newest* hit of that cost; nothing is removed when no hit of that
    /// cost is live, so a refund never credits a balance that was not charged.
    pub fn release(&mut self, cost: f64, now: Instant) {
        self.prune(now);
        if let Some(index) = self
            .hits
            .iter()
            .rposition(|(_, hit_cost)| *hit_cost == cost)
            && let Some((_, hit_cost)) = self.hits.remove(index)
        {
            self.consumed -= hit_cost;
        }
    }

    /// Tokens still available inside the window at `now`.
    #[must_use]
    pub fn remaining(&self, now: Instant) -> f64 {
        (self.capacity - self.consumed_at(now)).max(0.0)
    }

    fn consumed_at(&self, now: Instant) -> f64 {
        self.hits
            .iter()
            .filter(|(at, _)| now.checked_duration_since(*at).unwrap_or_default() < self.window)
            .map(|(_, cost)| cost)
            .sum()
    }

    /// Duration until `cost` tokens fit inside the window, or
    /// [`Duration::MAX`] when they never will.
    #[must_use]
    pub fn time_to_tokens(&self, cost: f64, now: Instant) -> Duration {
        if self.consumed + cost <= self.capacity {
            return Duration::ZERO;
        }
        let mut accrued = 0.0;
        for (at, hit_cost) in &self.hits {
            if now.checked_duration_since(*at).unwrap_or_default() >= self.window {
                continue;
            }
            accrued += hit_cost;
            if self.consumed - accrued + cost <= self.capacity {
                let wait = at
                    .checked_add(self.window)
                    .and_then(|expires| expires.checked_duration_since(now));
                return wait.unwrap_or(Duration::MAX);
            }
        }
        Duration::MAX
    }

    /// Duration until the window is empty again, i.e. the full capacity is
    /// available.
    #[must_use]
    pub fn time_to_reset(&self, now: Instant) -> Duration {
        self.hits
            .front()
            .and_then(|(at, _)| {
                at.checked_add(self.window)
                    .and_then(|expires| expires.checked_duration_since(now))
            })
            .unwrap_or(Duration::ZERO)
    }
}

// ---------------------------------------------------------------------------
// Decision
// ---------------------------------------------------------------------------

/// Outcome of one rate-limit check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitDecision {
    /// `true` when the request may proceed.
    pub allowed: bool,
    /// Configured limit (`X-RateLimit-Limit`).
    pub limit: u64,
    /// Tokens left after this request (`X-RateLimit-Remaining`).
    pub remaining: u64,
    /// Absolute epoch second at which the budget resets
    /// (`X-RateLimit-Reset`).
    pub reset_epoch_secs: u64,
    /// `Retry-After` seconds for a rejected request.
    pub retry_after_seconds: Option<u64>,
    /// Bounded wait a `queue` strategy granted; the data plane awaits it.
    pub queue_wait: Option<Duration>,
    /// `true` when a `degrade` strategy served a throttled request.
    pub degraded: bool,
    /// Configured strategy.
    pub strategy: RateLimitStrategy,
    /// Whether `X-RateLimit-*` headers should be emitted.
    pub emit_headers: bool,
}

impl RateLimitDecision {
    /// `true` when the request may proceed (including degraded service).
    #[must_use]
    pub const fn is_allowed(&self) -> bool {
        self.allowed
    }

    /// `true` when this decision must be answered with `429`.
    #[must_use]
    pub const fn is_limited(&self) -> bool {
        !self.allowed
    }

    /// Response headers this decision contributes.
    ///
    /// Empty when `response_headers` is disabled in the configuration, so the
    /// data plane can append the result unconditionally.
    #[must_use]
    pub fn headers(&self) -> Vec<(HeaderName, HeaderValue)> {
        if !self.emit_headers {
            return Vec::new();
        }
        let mut headers = vec![
            header(RATE_LIMIT_HEADER, &self.limit.to_string()),
            header(RATE_LIMIT_REMAINING_HEADER, &self.remaining.to_string()),
            header(RATE_LIMIT_RESET_HEADER, &self.reset_epoch_secs.to_string()),
        ];
        if let Some(retry_after) = self.retry_after_seconds {
            headers.push(header(RETRY_AFTER_HEADER, &retry_after.to_string()));
        }
        if self.degraded {
            headers.push(header(DEGRADED_HEADER, DEGRADED_HEADER_VALUE));
        }
        headers
    }

    /// The `429` problem document for a rejected request (ADR-0003), carrying
    /// the `Retry-After` hint in the problem `context`.
    #[must_use]
    pub fn into_error(self) -> OagwError {
        let mut error = OagwError::rate_limit_exceeded(format!(
            "rate limit of {} requests per {} exceeded",
            self.limit,
            self.strategy_hint()
        ));
        if let Some(retry_after) = self.retry_after_seconds {
            error = error.with_retry_after_seconds(retry_after);
        }
        error
    }

    fn strategy_hint(&self) -> &'static str {
        match self.strategy {
            RateLimitStrategy::Reject => "the configured window",
            RateLimitStrategy::Queue => "the configured window (queued)",
            RateLimitStrategy::Degrade => "the configured window (degraded)",
        }
    }
}

/// Builds a single response header, dropping values that are not valid ASCII
/// header values (never the case for the numbers rendered here).
fn header(name: &'static str, value: &str) -> (HeaderName, HeaderValue) {
    let header_name = HeaderName::from_static(name);
    let header_value =
        HeaderValue::from_str(value).unwrap_or_else(|_| HeaderValue::from_static("0"));
    (header_name, header_value)
}

// ---------------------------------------------------------------------------
// Scope + registry
// ---------------------------------------------------------------------------

/// Scope identifiers a caller supplies for one rate-limit check.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RateLimitScopeValues {
    /// Tenant id of the authenticated subject.
    pub tenant_id: Option<String>,
    /// Authenticated subject id.
    pub subject_id: Option<String>,
    /// Client address for the `ip` scope: the peer socket address when the
    /// transport supplies one, otherwise the last `X-Forwarded-For` entry (the
    /// data plane documents why the header is only ever a fallback).
    pub peer_ip: Option<String>,
    /// Matched route id.
    pub route_id: Option<String>,
}

impl RateLimitScopeValues {
    /// The value the configured scope resolves to, falling back to the tenant
    /// id and then to a shared bucket so a missing scope value can never
    /// widen the limit.
    #[must_use]
    pub fn identifier(&self, scope: RateLimitScope) -> String {
        let candidate = match scope {
            RateLimitScope::Global => None,
            RateLimitScope::Tenant => self.tenant_id.clone(),
            RateLimitScope::User => self.subject_id.clone().or_else(|| self.tenant_id.clone()),
            RateLimitScope::Ip => self.peer_ip.clone().or_else(|| self.tenant_id.clone()),
            RateLimitScope::Route => self.route_id.clone().or_else(|| self.tenant_id.clone()),
        };
        candidate.unwrap_or_else(|| "unscoped".to_owned())
    }
}

/// One limiter instance held by the registry.
#[derive(Debug, Clone)]
enum Limiter {
    Bucket(TokenBucket),
    Window(SlidingWindow),
}

impl Limiter {
    fn matches(&self, effective: &EffectiveRateLimit) -> bool {
        match self {
            Self::Bucket(bucket) => {
                bucket.capacity == effective.capacity_f64()
                    && bucket.refill_rate == effective.refill_rate()
            }
            Self::Window(window) => window.capacity == effective.capacity_f64(),
        }
    }

    /// `true` when this limiter is the algorithm `effective` configures, which
    /// a refund has to confirm before it credits a balance: a configuration
    /// change that only keeps the capacity would otherwise hand the tokens of
    /// the old bucket to the new one.
    fn holds(&self, algorithm: RateLimitAlgorithm) -> bool {
        matches!(
            (self, algorithm),
            (Self::Bucket(_), RateLimitAlgorithm::TokenBucket)
                | (Self::Window(_), RateLimitAlgorithm::SlidingWindow)
        )
    }

    fn release(&mut self, cost: f64, now: Instant) {
        match self {
            Self::Bucket(bucket) => bucket.release(cost, now),
            Self::Window(window) => window.release(cost, now),
        }
    }
}

/// A limiter together with the instant its bucket was last consulted, which is
/// what the eviction of [`MAX_BUCKETS`] orders on.
#[derive(Debug, Clone)]
struct Entry {
    limiter: Limiter,
    last_used: Instant,
}

/// Per-(upstream, scope) limiter registry.
///
/// Counters are process-local (ADR-0003 MVP: "Per-instance rate limiting in
/// Data Plane"), keyed by `ratelimit:{upstream_id}:{scope}:{identifier}` so a
/// configuration change to one upstream never touches another's buckets. The
/// registry holds at most [`MAX_BUCKETS`] buckets and drops the least recently
/// used eighth beyond that, so a bucket-per-client key space cannot grow the
/// registry without bound.
#[derive(Debug, Default)]
pub struct RateLimiterRegistry {
    limiters: DashMap<String, Entry>,
}

impl RateLimiterRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            limiters: DashMap::new(),
        }
    }

    /// Number of live limiters.
    #[must_use]
    pub fn len(&self) -> usize {
        self.limiters.len()
    }

    /// `true` when no limiter has been created yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.limiters.is_empty()
    }

    /// Drops every limiter, e.g. when the upstream it belongs to is deleted.
    pub fn clear(&self) {
        self.limiters.clear();
    }

    /// Drops the limiters of one upstream.
    pub fn clear_upstream(&self, upstream_id: Uuid) {
        let prefix = format!("ratelimit:{upstream_id}:");
        self.limiters.retain(|key, _| !key.starts_with(&prefix));
    }

    /// Counter key of one `(upstream, scope)` pair.
    ///
    /// Two decisions live in this key. First, the client half of an `ip` scope
    /// is the *peer socket address* whenever the transport supplies one, and
    /// only falls back to the last `X-Forwarded-For` entry — a client-controlled
    /// header must never mint a bucket of its own, or the limit is bypassed by
    /// rotating the header and a caller can spend another caller's budget (the
    /// data plane derives the value in its `client_identity` helper and the
    /// trade-off is documented there). Second, the key space is not under the
    /// operator's control, which is why [`check`] bounds the number of buckets
    /// it creates.
    #[must_use]
    pub fn scope_key(
        &self,
        upstream_id: Uuid,
        scope: RateLimitScope,
        values: &RateLimitScopeValues,
    ) -> String {
        format!(
            "ratelimit:{upstream_id}:{scope:?}:{}",
            values.identifier(scope)
        )
    }

    /// Runs one check and returns the decision (and consumes the tokens).
    ///
    /// Creating a bucket may evict: the registry is bounded by [`MAX_BUCKETS`],
    /// and the least recently used eighth is dropped wholesale once the bound is
    /// reached. An evicted bucket starts full again, which is why eviction only
    /// ever happens in a batch far larger than the steady-state request rate of
    /// one key.
    #[must_use]
    pub fn check(
        &self,
        upstream_id: Uuid,
        effective: &EffectiveRateLimit,
        values: &RateLimitScopeValues,
        now: Instant,
        epoch_now: u64,
    ) -> RateLimitDecision {
        let key = self.scope_key(upstream_id, effective.scope, values);
        let decision = {
            let mut entry = self.limiters.entry(key).or_insert_with(|| Entry {
                limiter: effective.clone().into_limiter(now),
                last_used: now,
            });
            entry.value_mut().last_used = now;
            if !entry.value().limiter.matches(effective) {
                entry.value_mut().limiter = effective.clone().into_limiter(now);
            }
            entry
                .value_mut()
                .limiter
                .evaluate(effective, now, epoch_now)
        };
        self.evict();
        decision
    }

    /// Captures the refund handle of a granted `queue` reservation.
    ///
    /// `None` unless the decision actually debited the bucket *and* queued:
    /// a request admitted outright holds no reservation, and a `429` holds no
    /// debit. The second half matters for a sliding window, whose `429` from a
    /// full queue carries a `queue_wait` (the projected wait) without having
    /// recorded a hit — a handle for it would refund a hit some *other* request
    /// paid for.
    #[must_use]
    pub fn reservation(
        &self,
        upstream_id: Uuid,
        effective: &EffectiveRateLimit,
        values: &RateLimitScopeValues,
        decision: &RateLimitDecision,
    ) -> Option<RateLimitReservation> {
        if !decision.allowed {
            return None;
        }
        decision.queue_wait?;
        Some(RateLimitReservation {
            key: self.scope_key(upstream_id, effective.scope, values),
            effective: effective.clone(),
            cost: effective.cost_f64(),
        })
    }

    /// Refunds a [`RateLimitReservation`], putting the debited tokens back.
    ///
    /// A bucket that no longer exists (evicted) or that no longer matches the
    /// configuration the reservation was granted under (rebuilt by a
    /// configuration change) holds no debit, so it is left alone — a refund
    /// must never credit a balance that was not charged.
    pub fn release(&self, reservation: RateLimitReservation, now: Instant) {
        let Some(mut entry) = self.limiters.get_mut(&reservation.key) else {
            return;
        };
        entry.value_mut().last_used = now;
        if entry.value().limiter.holds(reservation.effective.algorithm)
            && entry.value().limiter.matches(&reservation.effective)
        {
            entry.value_mut().limiter.release(reservation.cost, now);
        }
    }

    /// Drops the least recently used buckets when the registry reached its
    /// bound; a no-op below [`MAX_BUCKETS`].
    fn evict(&self) {
        if self.limiters.len() < MAX_BUCKETS {
            return;
        }
        let mut candidates: Vec<(String, Instant)> = self
            .limiters
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().last_used))
            .collect();
        candidates.sort_by_key(|(_, last_used)| *last_used);
        for (key, _) in candidates.into_iter().take(EVICTION_BATCH) {
            self.limiters.remove(&key);
        }
        tracing::debug!(
            target: "oagw.rate_limit",
            buckets = self.limiters.len(),
            "rate-limit registry reached its bound and evicted the least recently used buckets"
        );
    }
}

/// The budget a granted `queue` reservation holds, ready to be refunded.
///
/// A `queue` strategy debits the bucket up front (the token bucket even below
/// zero) and the data plane spends the granted wait before forwarding. When the
/// request then fails — an oversized body, a plugin rejection, an unreachable
/// upstream — the debit must not be lost, or every failed request silently
/// consumes budget no request was served for.
///
/// [`RateLimiterRegistry::reservation`] captures the handle and
/// [`RateLimiterRegistry::release`] refunds it; dropping the value without
/// releasing simply leaves the debit in place.
#[derive(Debug, Clone)]
pub struct RateLimitReservation {
    /// Resolved limiter key (`ratelimit:{upstream_id}:{scope}:{identifier}`),
    /// so the refund lands on exactly the bucket the debit came from.
    key: String,
    /// Configuration the reservation was granted under.
    effective: EffectiveRateLimit,
    /// Tokens to put back.
    cost: f64,
}

impl EffectiveRateLimit {
    fn into_limiter(self, now: Instant) -> Limiter {
        match self.algorithm {
            RateLimitAlgorithm::TokenBucket => Limiter::Bucket(TokenBucket::new(
                self.capacity_f64(),
                self.refill_rate(),
                now,
            )),
            RateLimitAlgorithm::SlidingWindow => Limiter::Window(SlidingWindow::new(
                self.capacity_f64(),
                self.window_duration(),
            )),
        }
    }
}

impl Limiter {
    /// Applies the configured strategy and renders the decision.
    fn evaluate(
        &mut self,
        effective: &EffectiveRateLimit,
        now: Instant,
        epoch_now: u64,
    ) -> RateLimitDecision {
        match &mut *self {
            Self::Bucket(bucket) => bucket.evaluate(effective, now, epoch_now),
            Self::Window(window) => window.evaluate(effective, now, epoch_now),
        }
    }
}

/// Rounds a duration up to whole seconds, never below one second: a client
/// told to retry in `0s` would immediately hammer the gateway again.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the value is clamped non-negative before truncating to whole seconds"
)]
fn retry_after_seconds(wait: Duration) -> u64 {
    let secs = wait.as_secs_f64().ceil();
    if secs < 1.0 {
        1
    } else if secs > f64::from(u32::MAX) {
        u64::from(u32::MAX)
    } else {
        secs as u64
    }
}

/// Renders an absolute epoch second for a relative duration.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "epoch seconds plus a bounded duration always fit u64"
)]
fn reset_epoch(epoch_now: u64, wait: Duration) -> u64 {
    epoch_now.saturating_add(retry_after_seconds(wait))
}

impl TokenBucket {
    fn evaluate(
        &mut self,
        effective: &EffectiveRateLimit,
        now: Instant,
        epoch_now: u64,
    ) -> RateLimitDecision {
        let cost = effective.cost_f64();
        let limit = effective.capacity.max(effective.sustained_rate);
        let reset = reset_epoch(
            epoch_now,
            self.time_to_tokens(effective.capacity_f64(), now),
        );
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "remaining quota is reported as whole tokens and clamped non-negative"
        )]
        let shape = |allowed: bool,
                     remaining: f64,
                     retry_after: Option<u64>,
                     queue_wait: Option<Duration>,
                     degraded: bool| {
            RateLimitDecision {
                allowed,
                limit,
                remaining: remaining.max(0.0) as u64,
                reset_epoch_secs: reset,
                retry_after_seconds: retry_after,
                queue_wait,
                degraded,
                strategy: effective.strategy,
                emit_headers: effective.response_headers,
            }
        };

        match effective.strategy {
            RateLimitStrategy::Reject => {
                if self.try_acquire(cost, now) {
                    shape(true, self.tokens, None, None, false)
                } else {
                    let wait = self.time_to_tokens(cost, now);
                    shape(
                        false,
                        self.tokens,
                        Some(retry_after_seconds(wait)),
                        None,
                        false,
                    )
                }
            }
            RateLimitStrategy::Queue => {
                let wait = self.time_to_tokens(cost, now);
                if wait <= QUEUE_MAX_WAIT {
                    self.acquire_credit(cost, now);
                    let queued_wait = if wait.is_zero() { None } else { Some(wait) };
                    shape(true, self.tokens.max(0.0), None, queued_wait, false)
                } else {
                    shape(
                        false,
                        self.tokens.max(0.0),
                        Some(retry_after_seconds(wait)),
                        None,
                        false,
                    )
                }
            }
            RateLimitStrategy::Degrade => {
                // Degraded service never rejects: the request is served and
                // flagged instead (see the module docs).
                let served = self.acquire_partial(cost, now);
                shape(true, self.tokens, None, None, !served)
            }
        }
    }
}

impl SlidingWindow {
    fn evaluate(
        &mut self,
        effective: &EffectiveRateLimit,
        now: Instant,
        epoch_now: u64,
    ) -> RateLimitDecision {
        let cost = effective.cost_f64();
        let limit = effective.capacity.max(effective.sustained_rate);
        let reset = reset_epoch(epoch_now, self.time_to_reset(now));

        match effective.strategy {
            RateLimitStrategy::Reject => {
                if self.try_acquire(cost, now) {
                    self.decision(
                        true,
                        limit,
                        self.remaining(now),
                        reset,
                        None,
                        None,
                        false,
                        effective,
                    )
                } else {
                    let wait = self.time_to_tokens(cost, now);
                    self.decision(
                        false,
                        limit,
                        self.remaining(now),
                        reset,
                        Some(retry_after_seconds(wait)),
                        None,
                        false,
                        effective,
                    )
                }
            }
            RateLimitStrategy::Queue => {
                let wait = self.time_to_tokens(cost, now);
                if wait <= QUEUE_MAX_WAIT {
                    let granted = self.try_acquire(cost, now);
                    // The window is at capacity, so the reservation this
                    // strategy grants cannot always be delivered: a request
                    // that is still denied must carry the ADR-0003
                    // `Retry-After` like any other `429`, not only a queue
                    // wait.
                    let retry_after = if granted {
                        None
                    } else {
                        Some(retry_after_seconds(wait))
                    };
                    self.decision(
                        granted,
                        limit,
                        self.remaining(now),
                        reset,
                        retry_after,
                        if wait.is_zero() { None } else { Some(wait) },
                        false,
                        effective,
                    )
                } else {
                    self.decision(
                        false,
                        limit,
                        self.remaining(now),
                        reset,
                        Some(retry_after_seconds(wait)),
                        None,
                        false,
                        effective,
                    )
                }
            }
            RateLimitStrategy::Degrade => {
                let served = self.try_acquire(cost, now);
                self.decision(
                    true,
                    limit,
                    self.remaining(now),
                    reset,
                    None,
                    None,
                    !served,
                    effective,
                )
            }
        }
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the sliding window decision carries every ADR-0003 output field"
    )]
    fn decision(
        &self,
        allowed: bool,
        limit: u64,
        remaining: f64,
        reset_epoch_secs: u64,
        retry_after_seconds: Option<u64>,
        queue_wait: Option<Duration>,
        degraded: bool,
        effective: &EffectiveRateLimit,
    ) -> RateLimitDecision {
        RateLimitDecision {
            allowed,
            limit,
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "remaining quota is reported as whole tokens and clamped non-negative"
            )]
            remaining: remaining.max(0.0) as u64,
            reset_epoch_secs,
            retry_after_seconds,
            queue_wait,
            degraded,
            strategy: effective.strategy,
            emit_headers: effective.response_headers,
        }
    }
}

#[cfg(test)]
#[path = "rate_limit_tests.rs"]
mod tests;
