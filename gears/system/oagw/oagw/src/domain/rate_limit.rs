//! Rate limiting on the proxy path (ADR-0003).
//!
//! Every upstream and every route may carry a `rate_limit` document; a request
//! that matches both is admitted only when *both* counters admit it, which is
//! the ADR's hierarchical `min(parent, child)` rule expressed as two checks
//! instead of a merged limit — the parent never has to know about its children.
//!
//! # State ownership
//!
//! ADR-0003 "Distribution" makes per-instance, in-process limiting the MVP and
//! defers the Redis-backed hybrid sync to a later slice: "Rate limiting
//! executes in Data Plane (DP). Configuration is resolved from Control Plane
//! (CP) caches during upstream/route resolution." Accordingly the counters live
//! in a [`RateLimiter`] owned by the data plane, are keyed by the ADR's key
//! layout (minus the time-bucket segment, which only a fixed-window Redis
//! counter needs) and are swept when the map grows past [`MAX_COUNTERS`]: idle
//! counters first, then — a map full of live counters — the least recently
//! active ones. A
//! restart forgets every counter, which for a per-instance limiter is the
//! documented behaviour rather than a defect.
//!
//! # Deferred by the ADR itself
//!
//! * `strategy: queue` and `strategy: degrade` — the ADR lists them as
//!   configuration but defines no queue depth, no wait bound and no degraded
//!   response shape. Every configured strategy is enforced as `reject`, the
//!   documented default, and the 429 says so through the standard headers.
//! * the `budget` hierarchy (`total`, `overcommit_ratio`) and the
//!   `response_headers` switch — neither member exists in
//!   `docs/schemas/route.v1.schema.json` / `upstream.v1.schema.json`, so the
//!   headers below are always sent and no budget allocation is validated.
//! * distributed counters (`rate_limit_sync`).

use std::collections::HashMap;
use std::collections::VecDeque;
use std::collections::hash_map::Entry;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use http::{HeaderName, HeaderValue};
use parking_lot::Mutex;
use uuid::Uuid;

use crate::domain::model::{RateLimitAlgorithm, RateLimitConfig, RateLimitScope, RateLimitWindow};
use crate::error::{
    OagwError, RETRY_AFTER_HEADER, X_RATELIMIT_LIMIT_HEADER, X_RATELIMIT_REMAINING_HEADER,
    X_RATELIMIT_RESET_HEADER,
};

/// Fraction of a token the bucket tracks: one token is split into a billion
/// sub-units, so a refill can be applied exactly with integer arithmetic and a
/// partially refilled token is never handed out.
const NANOUNITS_PER_TOKEN: u128 = 1_000_000_000;

/// Number of distinct counters kept in memory before the idle ones are swept.
///
/// A counter is keyed by scope identity, so an attacker who can vary the key
/// (a different peer address per request, say) could otherwise grow the map
/// without bound. Sweeping keeps the footprint flat: a swept counter restarts
/// full, which is exactly the state its refill would have reached.
const MAX_COUNTERS: usize = 65_536;

/// Where a `now` comes from, so a test can step time instead of racing the
/// scheduler.
pub trait RateLimitClock: Send + Sync + 'static {
    /// The current instant.
    fn now(&self) -> Instant;
}

/// The production clock: [`Instant::now`].
#[derive(Debug, Clone, Copy, Default)]
pub struct MonotonicClock;

impl RateLimitClock for MonotonicClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Who a request is counted as (ADR-0003 "Scope of a limit").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitSubject {
    /// Tenant the proxy call was authenticated for.
    pub tenant_id: Uuid,
    /// Authenticated caller, when the request carried credentials.
    pub subject_id: Option<Uuid>,
    /// Client address, when the transport exposed one.
    pub peer: Option<IpAddr>,
    /// Route the request matched.
    pub route_id: Uuid,
}

/// A rate limit document bound to the resource that owns it, resolved once per
/// request instead of once per counter access.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitRule {
    resource: &'static str,
    resource_id: String,
    limit: EffectiveLimit,
}

impl RateLimitRule {
    /// The `rate_limit` document of an upstream.
    #[must_use]
    pub fn upstream(id: Uuid, config: &RateLimitConfig) -> Self {
        Self::new("upstream", id, config)
    }

    /// The `rate_limit` document of a route.
    #[must_use]
    pub fn route(id: Uuid, config: &RateLimitConfig) -> Self {
        Self::new("route", id, config)
    }

    fn new(resource: &'static str, id: Uuid, config: &RateLimitConfig) -> Self {
        Self {
            resource,
            resource_id: id.to_string(),
            limit: EffectiveLimit::of(config),
        }
    }

    /// The counter key the request is counted under (ADR-0003 "Redis key
    /// structure").
    #[must_use]
    pub fn key(&self, subject: &RateLimitSubject) -> String {
        scope_key(
            self.resource,
            &self.resource_id,
            self.limit.scope,
            subject,
            self.limit.window,
        )
    }

    /// The resolved limit.
    #[must_use]
    pub const fn limit(&self) -> &EffectiveLimit {
        &self.limit
    }

    /// The resource kind the limit belongs to (`upstream` or `route`).
    #[must_use]
    pub const fn resource(&self) -> &'static str {
        self.resource
    }
}

/// The counter key of a request: `oagw:ratelimit:{resource}:{resource_id}:{scope}:{scope_id}:{window}`.
///
/// The ADR's key ends in a time-bucket segment (`…:minute:202601301530`); the
/// segment exists so a Redis fixed-window counter can be expired per bucket and
/// an in-memory counter has no use for it.
///
/// A scope whose identity the request does not carry degrades to the tenant
/// counter rather than to a shared, keyless one. ADR-0003 lists the scopes and
/// says nothing about an identity the request does not carry, so the
/// degradation is this gear's decision: an unauthenticated caller gets no `user`
/// counter of its own and a request whose peer address is unknown gets no `ip`
/// counter, because either would be shared by every caller of the tenant and
/// would let one of them exhaust the budget of all the others.
#[must_use]
pub fn scope_key(
    resource: &str,
    resource_id: &str,
    scope: RateLimitScope,
    subject: &RateLimitSubject,
    window: RateLimitWindow,
) -> String {
    let scope_id = match scope {
        RateLimitScope::Global => "-".to_owned(),
        RateLimitScope::Tenant => subject.tenant_id.to_string(),
        RateLimitScope::Route => subject.route_id.to_string(),
        RateLimitScope::User => subject.subject_id.unwrap_or(subject.tenant_id).to_string(),
        RateLimitScope::Ip => subject
            .peer
            .map_or_else(|| subject.tenant_id.to_string(), |peer| peer.to_string()),
    };
    format!(
        "oagw:ratelimit:{resource}:{resource_id}:{}:{scope_id}:{}",
        scope.name(),
        window.name()
    )
}

/// A `rate_limit` document reduced to the numbers the algorithms use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffectiveLimit {
    /// Algorithm the counter runs.
    pub algorithm: RateLimitAlgorithm,
    /// Counter scope the key is built from.
    pub scope: RateLimitScope,
    /// Tokens replenished per `window`.
    pub rate: u64,
    /// Window unit `rate` is measured over.
    pub window: RateLimitWindow,
    /// Bucket capacity: the burst a caller may spend at once.
    pub capacity: u64,
    /// Tokens consumed per request.
    pub cost: u64,
}

impl EffectiveLimit {
    /// Resolve a document. A document without `burst` has the capacity of its
    /// sustained rate, and a bucket is never smaller than the cost of one
    /// request: a bucket that cannot pay for a single request could never admit
    /// one, which no caller configuring a limit can have meant.
    #[must_use]
    pub fn of(config: &RateLimitConfig) -> Self {
        let capacity = config
            .burst
            .map_or(config.sustained.rate, |burst| burst.capacity)
            .max(config.cost);
        Self {
            algorithm: config.algorithm,
            scope: config.scope,
            rate: config.sustained.rate,
            window: config.sustained.window,
            capacity,
            cost: config.cost,
        }
    }

    /// Length of the configured window.
    #[must_use]
    pub const fn window_length(&self) -> Duration {
        window_length(self.window)
    }

    /// Time one refill cycle of `deficit` tokens takes, as a duration.
    ///
    /// The division truncates a duration that is already a *rate* derived figure:
    /// a fraction of a nanosecond is below the resolution the retry header can
    /// carry, and the seconds are rounded up for the caller.
    fn deficit_duration(&self, deficit: u128) -> Duration {
        let per_second = u128::from(self.rate.max(1)) * NANOUNITS_PER_TOKEN;
        #[allow(
            clippy::integer_division,
            reason = "the remainder is a fraction of a nanosecond of refill time"
        )]
        let nanos = deficit.saturating_mul(u128::from(window_nanos(self.window))) / per_second;
        Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
    }
}

/// Length of a window unit.
const fn window_length(window: RateLimitWindow) -> Duration {
    match window {
        RateLimitWindow::Second => Duration::from_secs(1),
        RateLimitWindow::Minute => Duration::from_mins(1),
        RateLimitWindow::Hour => Duration::from_hours(1),
        RateLimitWindow::Day => Duration::from_hours(24),
    }
}

/// Length of a window unit in nanoseconds, the unit the refill arithmetic uses.
const fn window_nanos(window: RateLimitWindow) -> u64 {
    match window {
        RateLimitWindow::Second => 1_000_000_000,
        RateLimitWindow::Minute => 60_000_000_000,
        RateLimitWindow::Hour => 3_600_000_000_000,
        RateLimitWindow::Day => 86_400_000_000_000,
    }
}

/// What an admitted request spent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grant {
    /// Tokens still available after the request.
    pub remaining: u64,
    /// The advertised limit (`X-RateLimit-Limit`): the sustained rate.
    pub limit: u64,
    /// Seconds until the counter is back to its full budget.
    pub reset_seconds: u64,
}

/// Why a request was refused, and when trying again can succeed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rejection {
    /// Time until the request would be admitted.
    pub retry_after: Duration,
    /// Tokens available at the moment of the refusal.
    pub remaining: u64,
    /// The advertised limit (`X-RateLimit-Limit`): the sustained rate.
    pub limit: u64,
    /// Seconds until the counter is back to its full budget.
    pub reset_seconds: u64,
}

impl Rejection {
    /// The 429 problem document for this refusal (ADR-0003 "429 Response",
    /// ADR-0007): the rate limit headers travel as response headers, the retry
    /// guidance additionally as a problem extension member.
    #[must_use]
    pub fn to_error(self, resource: &str, limit: &EffectiveLimit) -> OagwError {
        let seconds = retry_after_seconds(self.retry_after);
        let mut error = OagwError::rate_limit_exceeded(format!(
            "the rate limit of {} tokens per {} for the {resource} is exhausted: {} of them may be \
             spent at once and the request costs {}",
            limit.rate,
            limit.window.name(),
            limit.capacity,
            limit.cost
        ))
        .with_retry_after_seconds(seconds);
        for (name, value) in self.headers(seconds) {
            error = error.with_response_header(name, value);
        }
        error
    }

    /// The rate limit headers of the refusal.
    fn headers(self, seconds: u64) -> Vec<(HeaderName, HeaderValue)> {
        vec![
            (RETRY_AFTER_HEADER, header_value(&seconds.to_string())),
            (
                X_RATELIMIT_LIMIT_HEADER,
                header_value(&self.limit.to_string()),
            ),
            (
                X_RATELIMIT_REMAINING_HEADER,
                header_value(&self.remaining.to_string()),
            ),
            (
                X_RATELIMIT_RESET_HEADER,
                header_value(&reset_epoch(self.reset_seconds)),
            ),
        ]
    }
}

/// Result of one counter check.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitOutcome {
    /// The request was admitted.
    Allowed(Grant),
    /// The request was refused.
    Rejected(Rejection),
}

impl RateLimitOutcome {
    /// Whether the request was admitted.
    #[must_use]
    pub const fn is_allowed(&self) -> bool {
        matches!(self, Self::Allowed(_))
    }

    /// Whether the request was refused.
    #[must_use]
    pub const fn is_rejected(&self) -> bool {
        matches!(self, Self::Rejected(_))
    }

    /// Split the outcome into the grant or the refusal it carries, for a caller
    /// that only cares about one side.
    ///
    /// # Errors
    ///
    /// Returns the [`Rejection`] when the counter refused the request.
    pub fn allowed(self) -> Result<Grant, Rejection> {
        match self {
            Self::Allowed(grant) => Ok(grant),
            Self::Rejected(rejection) => Err(rejection),
        }
    }
}

/// The state of one counter, keyed by [`scope_key`].
#[derive(Debug)]
struct Counter {
    algorithm: RateLimitAlgorithm,
    window: RateLimitWindow,
    /// Nanounits of budget the token bucket holds, refilled lazily.
    tokens: u128,
    /// Instant the token bucket was last refilled at.
    last_refill: Instant,
    /// Instants the sliding window charged, oldest first.
    acquisitions: VecDeque<Instant>,
    /// Last request the counter saw, for the idle sweep.
    last_activity: Instant,
}

impl Counter {
    fn fresh(limit: &EffectiveLimit, now: Instant) -> Self {
        Self {
            algorithm: limit.algorithm,
            window: limit.window,
            tokens: u128::from(limit.capacity) * NANOUNITS_PER_TOKEN,
            last_refill: now,
            acquisitions: VecDeque::new(),
            last_activity: now,
        }
    }

    /// Whether the counter can serve a limit without being rebuilt: a counter
    /// holds no configuration of its own beyond the algorithm and the window it
    /// was created for, so a reconfigured limit takes effect on the next
    /// request instead of at some expiry.
    fn matches(&self, limit: &EffectiveLimit) -> bool {
        self.algorithm == limit.algorithm && self.window == limit.window
    }

    /// How long the counter is worth keeping once nothing hits it: after one
    /// window a sliding window is empty again, and a token bucket has refilled
    /// to its capacity, so a fresh counter is indistinguishable from the old one.
    fn idle_ttl(&self) -> Duration {
        window_length(self.window)
    }

    /// The token-bucket check (ADR-0003 "Token Bucket algorithm"): refill from
    /// `last_refill`, clamp to the capacity, then spend `cost`.
    fn acquire_tokens(&mut self, limit: &EffectiveLimit, now: Instant) -> RateLimitOutcome {
        let capacity = u128::from(limit.capacity) * NANOUNITS_PER_TOKEN;
        let elapsed = now.saturating_duration_since(self.last_refill);
        self.tokens = (self.tokens + refill_nanounits(elapsed, limit)).min(capacity);
        self.last_refill = now;

        let needed = u128::from(limit.cost) * NANOUNITS_PER_TOKEN;
        let remaining = whole_tokens(self.tokens);
        if self.tokens >= needed {
            self.tokens -= needed;
            RateLimitOutcome::Allowed(Grant {
                remaining: whole_tokens(self.tokens),
                limit: limit.rate,
                reset_seconds: limit.deficit_duration(capacity - self.tokens).as_secs(),
            })
        } else {
            let retry_after = limit.deficit_duration(needed - self.tokens);
            RateLimitOutcome::Rejected(Rejection {
                retry_after,
                remaining,
                limit: limit.rate,
                reset_seconds: retry_after.as_secs(),
            })
        }
    }

    /// The sliding-window check (ADR-0003 "Sliding Window algorithm"): drop the
    /// acquisitions that left the window, then take a slot while one is free.
    fn acquire_slot(&mut self, limit: &EffectiveLimit, now: Instant) -> RateLimitOutcome {
        let window = limit.window_length();
        let capacity = usize::try_from(limit.capacity).unwrap_or(usize::MAX);
        while self
            .acquisitions
            .front()
            .is_some_and(|oldest| now.saturating_duration_since(*oldest) >= window)
        {
            self.acquisitions.pop_front();
        }

        let remaining = limit
            .capacity
            .saturating_sub(u64::try_from(self.acquisitions.len()).unwrap_or(u64::MAX));
        if self.acquisitions.len() < capacity {
            self.acquisitions.push_back(now);
            RateLimitOutcome::Allowed(Grant {
                remaining: remaining.saturating_sub(1),
                limit: limit.rate,
                reset_seconds: window.as_secs(),
            })
        } else {
            let retry_after = self.acquisitions.front().map_or(Duration::ZERO, |oldest| {
                window.saturating_sub(now.saturating_duration_since(*oldest))
            });
            RateLimitOutcome::Rejected(Rejection {
                retry_after,
                remaining,
                limit: limit.rate,
                reset_seconds: retry_after.as_secs(),
            })
        }
    }
}

/// Nanounits a refill of `elapsed` adds, at `rate` tokens per window.
fn refill_nanounits(elapsed: Duration, limit: &EffectiveLimit) -> u128 {
    #[allow(
        clippy::integer_division,
        reason = "the remainder is a fraction of a nanounit of budget, below one token"
    )]
    let refilled = elapsed
        .as_nanos()
        .saturating_mul(u128::from(limit.rate))
        .saturating_mul(NANOUNITS_PER_TOKEN)
        / u128::from(window_nanos(limit.window));
    refilled
}

/// Whole tokens held by a nanounit balance.
fn whole_tokens(nanounits: u128) -> u64 {
    #[allow(
        clippy::integer_division,
        reason = "the remainder is the unspent fraction of a token the bucket keeps"
    )]
    let tokens = nanounits / NANOUNITS_PER_TOKEN;
    u64::try_from(tokens).unwrap_or(u64::MAX)
}

/// Whole seconds a caller is told to wait, rounded up: a caller that waits
/// exactly that long must find the counter ready.
fn retry_after_seconds(retry_after: Duration) -> u64 {
    u64::from(retry_after.subsec_nanos() > 0).saturating_add(retry_after.as_secs())
}

/// The instant the counter recovers, as a Unix timestamp in seconds
/// (`X-RateLimit-Reset` in the ADR's response example).
fn reset_epoch(reset_seconds: u64) -> String {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (elapsed + Duration::from_secs(reset_seconds))
        .as_secs()
        .to_string()
}

/// A header value assembled from numbers and origin strings, all of them
/// visible ASCII, which is the only thing [`HeaderValue::from_str`] rejects.
fn header_value(value: &str) -> HeaderValue {
    HeaderValue::from_str(value).unwrap_or_else(|_| HeaderValue::from_static("-"))
}

/// The in-memory rate limiter of one data plane instance.
///
/// Counters are held under a single lock: a request takes it once, refills and
/// spends, and releases it, so a check is a few integer operations and the
/// proxy path is never blocked by a background job.
pub struct RateLimiter {
    counters: Mutex<HashMap<String, Counter>>,
    clock: Arc<dyn RateLimitClock>,
}

impl std::fmt::Debug for RateLimiter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RateLimiter")
            .field("counters", &self.counters)
            .finish_non_exhaustive()
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl RateLimiter {
    /// A limiter on the wall clock.
    #[must_use]
    pub fn new() -> Self {
        Self::with_clock(Arc::new(MonotonicClock))
    }

    /// A limiter on a caller-provided clock, for tests that step time.
    #[must_use]
    pub fn with_clock(clock: Arc<dyn RateLimitClock>) -> Self {
        Self {
            counters: Mutex::new(HashMap::new()),
            clock,
        }
    }

    /// Count one request against one limit.
    pub fn check(&self, key: &str, limit: &EffectiveLimit) -> RateLimitOutcome {
        let now = self.clock.now();
        let mut counters = self.counters.lock();
        let counter = counter_for(&mut counters, key, limit, now);
        counter.last_activity = now;
        match limit.algorithm {
            RateLimitAlgorithm::TokenBucket => counter.acquire_tokens(limit, now),
            RateLimitAlgorithm::SlidingWindow => counter.acquire_slot(limit, now),
        }
    }

    /// Count one request against every limit that applies to it, in the order
    /// the rules were built: an upstream limit is checked before the route
    /// limit, so a route limit that is not the binding constraint still sees
    /// the request and every counter stays in step with the traffic it served.
    ///
    /// # Errors
    ///
    /// Returns the 429 [`OagwError`] of the first counter that refused the
    /// request. Refused requests are not refunded to the counters they were
    /// charged to: a caller that retries pays again, which is what keeps the
    /// counters an honest measure of the work the gateway performed.
    pub fn enforce(
        &self,
        rules: &[RateLimitRule],
        subject: &RateLimitSubject,
    ) -> Result<(), OagwError> {
        for rule in rules {
            let outcome = self.check(rule.key(subject).as_str(), rule.limit());
            if let RateLimitOutcome::Rejected(rejection) = outcome {
                return Err(rejection.to_error(rule.resource(), rule.limit()));
            }
        }
        Ok(())
    }
}

/// The counter a key maps to, building or rebuilding it when needed.
fn counter_for<'a>(
    counters: &'a mut HashMap<String, Counter>,
    key: &str,
    limit: &EffectiveLimit,
    now: Instant,
) -> &'a mut Counter {
    let reusable = counters
        .get(key)
        .is_some_and(|counter| counter.matches(limit));
    if !reusable && counters.len() >= MAX_COUNTERS {
        sweep(counters, now);
    }
    match counters.entry(key.to_owned()) {
        Entry::Occupied(entry) if entry.get().matches(limit) => entry.into_mut(),
        Entry::Occupied(mut entry) => {
            entry.insert(Counter::fresh(limit, now));
            entry.into_mut()
        }
        Entry::Vacant(entry) => entry.insert(Counter::fresh(limit, now)),
    }
}

/// Drop every counter that has not been hit within its own window, and — when
/// even the live ones fill the map — the least recently active of them.
///
/// The idle pass is the normal case: a counter that has not been touched within
/// its own window is dead weight, whatever its bucket still holds. A map full of
/// *live* counters (65 536 scopes each hit within their window) still has to
/// stay bounded, so the second pass evicts by recency until one slot is free.
/// A counter evicted there restarts full, which is exactly the state its refill
/// would have reached, and a caller whose scope is evicted is merely re-limited
/// from a fresh budget — never let through.
fn sweep(counters: &mut HashMap<String, Counter>, now: Instant) {
    counters.retain(|_, counter| {
        now.saturating_duration_since(counter.last_activity) < counter.idle_ttl()
    });
    if counters.len() < MAX_COUNTERS {
        return;
    }
    let mut recency: Vec<(String, Instant)> = counters
        .iter()
        .map(|(key, counter)| (key.clone(), counter.last_activity))
        .collect();
    recency.sort_unstable_by_key(|(_, last)| *last);
    let excess = counters.len() + 1 - MAX_COUNTERS;
    for (key, _) in recency.into_iter().take(excess) {
        counters.remove(&key);
    }
}

#[cfg(test)]
#[path = "rate_limit_tests.rs"]
mod tests;
