//! In-memory rate limiting
//! ([ADR-0003](../../../docs/ADR/0003-rate-limiting.md)).
//!
//! [`RateLimiter`] is the local half of ADR-0003's hybrid distribution: a token
//! bucket (or sliding window) per ADR-0003 key, held in process memory. No
//! Redis client is wired in this build, so a counter is accurate for this node
//! only — the keys are laid out exactly as the ADR specifies, so a shared
//! backend can take the same state over without a config migration.
//!
//! Time is injected through [`RateLimitClock`]: the data plane runs on
//! [`SystemClock`] and the tests on a [`ManualClock`] they advance by hand, so
//! refill behaviour is verified without sleeping.
//!
//! A rejected request changes nothing but the clock of the bucket it hit: no
//! token is consumed, so a client that keeps retrying never starves the bucket
//! further.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use http::header::RETRY_AFTER;
use http::{HeaderMap, HeaderName, HeaderValue};
use parking_lot::Mutex;
use uuid::Uuid;

use crate::domain::error::OagwError;
use crate::domain::merger::EffectiveRateLimit;
use crate::domain::model::{
    RateLimitAlgorithm, RateLimitScope, RateLimitStrategy, RateLimitWindow,
};

#[cfg(test)]
#[path = "ratelimit_tests.rs"]
mod tests;

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

/// Prefix of every rate-limit key
/// ([ADR-0003](../../../docs/ADR/0003-rate-limiting.md) "Redis key structure").
pub const RATE_LIMIT_KEY_PREFIX: &str = "oagw:ratelimit:";

/// `X-RateLimit-Limit` — the merged limit the counter enforces.
pub const RATE_LIMIT_LIMIT_HEADER: &str = "x-ratelimit-limit";
/// `X-RateLimit-Remaining` — requests left after this one.
pub const RATE_LIMIT_REMAINING_HEADER: &str = "x-ratelimit-remaining";
/// `X-RateLimit-Reset` — Unix seconds of the instant the counter resets.
pub const RATE_LIMIT_RESET_HEADER: &str = "x-ratelimit-reset";

/// What a limit is attached to: the `{resource_type}:{resource_id}` prefix of
/// the ADR-0003 key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LimitResource {
    /// Kind of the resource the limit is configured on.
    pub kind: LimitResourceKind,
    /// Id of the resource.
    pub id: Uuid,
}

impl LimitResource {
    /// A limit configured on an upstream.
    #[must_use]
    pub const fn upstream(id: Uuid) -> Self {
        Self {
            kind: LimitResourceKind::Upstream,
            id,
        }
    }

    /// A limit configured on a route.
    #[must_use]
    pub const fn route(id: Uuid) -> Self {
        Self {
            kind: LimitResourceKind::Route,
            id,
        }
    }

    /// The ADR-0003 prefix every key of this resource shares.
    #[must_use]
    fn key_prefix(self) -> String {
        format!(
            "{}{}:{}",
            RATE_LIMIT_KEY_PREFIX,
            self.kind.as_str(),
            self.id
        )
    }
}

/// The `{resource_type}` segment of the ADR-0003 key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LimitResourceKind {
    /// The limit is configured on an upstream.
    Upstream,
    /// The limit is configured on a route.
    Route,
}

impl LimitResourceKind {
    /// The segment as the ADR-0003 example spells it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Upstream => "upstream",
            Self::Route => "route",
        }
    }
}

/// The `{scope}` of the ADR-0003 key: what the counter is keyed by.
///
/// The configured `rate_limit.scope` names the same idea with slightly
/// different words — `user` keys by subject — and the rest map one to one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LimitScope {
    /// One counter for the whole node.
    Global,
    /// One counter per tenant.
    Tenant,
    /// One counter per authenticated caller.
    Subject,
    /// One counter per client IP.
    Ip,
    /// One counter per route.
    Route,
    /// One counter per upstream.
    Upstream,
}

impl LimitScope {
    /// The scope a configured `rate_limit.scope` keys by.
    #[must_use]
    pub const fn of(scope: RateLimitScope) -> Self {
        match scope {
            RateLimitScope::Global => Self::Global,
            RateLimitScope::Tenant => Self::Tenant,
            RateLimitScope::User => Self::Subject,
            RateLimitScope::Ip => Self::Ip,
            RateLimitScope::Route => Self::Route,
        }
    }

    /// The `{scope}` segment of the key.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Tenant => "tenant",
            Self::Subject => "subject",
            Self::Ip => "ip",
            Self::Route => "route",
            Self::Upstream => "upstream",
        }
    }

    /// The `{scope_id}` segment of the key.
    ///
    /// Every scope id is rooted in the tenant id, so a key always names the
    /// tenant it belongs to whatever scope the limit declared: a `global`
    /// counter never lets one tenant's traffic drain another's.
    fn scope_id(self, identity: &RateLimitIdentity) -> String {
        let tenant = identity.tenant_id.to_string();
        match self {
            Self::Global | Self::Tenant => tenant,
            Self::Upstream => format!("{tenant}/{}", identity.upstream_id),
            Self::Subject => qualified(&tenant, identity.subject_id.map(|id| id.to_string())),
            Self::Ip => qualified(&tenant, identity.client_ip.map(|ip| ip.to_string())),
            Self::Route => qualified(&tenant, identity.route_id.map(|id| id.to_string())),
        }
    }
}

/// `{tenant}/{id}` when the request carries the scoped identity, `{tenant}`
/// when it does not: the counter stays keyed per tenant either way.
fn qualified(tenant: &str, id: Option<String>) -> String {
    match id {
        Some(id) => format!("{tenant}/{id}"),
        None => tenant.to_owned(),
    }
}

/// The identity a counter is keyed by, resolved per request by the data plane.
#[derive(Debug, Clone)]
pub struct RateLimitIdentity {
    /// Tenant the request was authenticated against: part of every key.
    pub tenant_id: Uuid,
    /// Upstream the request resolved to.
    pub upstream_id: Uuid,
    /// Route the request matched, when one did.
    pub route_id: Option<Uuid>,
    /// Authenticated caller, for the `subject` scope.
    pub subject_id: Option<Uuid>,
    /// Caller IP, for the `ip` scope.
    pub client_ip: Option<IpAddr>,
}

impl RateLimitIdentity {
    /// Builds an identity for `tenant_id` calling `upstream_id`.
    #[must_use]
    pub fn new(tenant_id: Uuid, upstream_id: Uuid) -> Self {
        Self {
            tenant_id,
            upstream_id,
            route_id: None,
            subject_id: None,
            client_ip: None,
        }
    }

    /// Sets the matched route.
    #[must_use]
    pub fn with_route(mut self, route_id: Uuid) -> Self {
        self.route_id = Some(route_id);
        self
    }

    /// Sets the authenticated caller.
    #[must_use]
    pub fn with_subject(mut self, subject_id: Uuid) -> Self {
        self.subject_id = Some(subject_id);
        self
    }

    /// Sets the client IP.
    #[must_use]
    pub fn with_client_ip(mut self, client_ip: IpAddr) -> Self {
        self.client_ip = Some(client_ip);
        self
    }
}

/// Builds the ADR-0003 key
/// `oagw:ratelimit:{resource_type}:{resource_id}:{scope}:{scope_id}:{window}`.
#[must_use]
pub fn rate_limit_key(
    resource: &LimitResource,
    scope: LimitScope,
    identity: &RateLimitIdentity,
    window: RateLimitWindow,
) -> String {
    format!(
        "{}:{}:{}:{}",
        resource.key_prefix(),
        scope.as_str(),
        scope.scope_id(identity),
        window_name(window),
    )
}

/// The `{window}` segment of the key, spelled as the schema spells it.
const fn window_name(window: RateLimitWindow) -> &'static str {
    match window {
        RateLimitWindow::Second => "second",
        RateLimitWindow::Minute => "minute",
        RateLimitWindow::Hour => "hour",
        RateLimitWindow::Day => "day",
    }
}

// ---------------------------------------------------------------------------
// Clock
// ---------------------------------------------------------------------------

/// The clock the limiter reads. Injected so that tests advance time by hand
/// instead of sleeping through a refill.
pub trait RateLimitClock: fmt::Debug + Send + Sync {
    /// The instant the refill is computed from.
    fn now(&self) -> Instant;

    /// Unix seconds of [`Self::now`], for the `X-RateLimit-Reset` header.
    fn unix_now(&self) -> u64;
}

/// The system clock, used by the data plane.
#[derive(Debug, Default)]
pub struct SystemClock;

impl RateLimitClock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn unix_now(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since_epoch| since_epoch.as_secs())
    }
}

/// A clock the tests advance by hand: deterministic time, no sleeps.
pub struct ManualClock {
    base: Instant,
    unix_base: u64,
    offset: Mutex<Duration>,
}

impl fmt::Debug for ManualClock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ManualClock")
            .field("unix", &self.unix_now())
            .finish()
    }
}

impl ManualClock {
    /// Starts a clock whose `X-RateLimit-Reset` values are readable against
    /// `unix`.
    #[must_use]
    pub fn start_at(unix: u64) -> Self {
        Self {
            base: Instant::now(),
            unix_base: unix,
            offset: Mutex::new(Duration::ZERO),
        }
    }

    /// Advances the clock.
    pub fn advance(&self, by: Duration) {
        *self.offset.lock() += by;
    }

    /// Advances the clock by whole seconds.
    pub fn advance_secs(&self, secs: u64) {
        self.advance(Duration::from_secs(secs));
    }
}

impl RateLimitClock for ManualClock {
    fn now(&self) -> Instant {
        self.base + *self.offset.lock()
    }

    fn unix_now(&self) -> u64 {
        self.unix_base + self.offset.lock().as_secs()
    }
}

// ---------------------------------------------------------------------------
// Counters
// ---------------------------------------------------------------------------

/// What one counter decided, before it becomes a [`RateLimitDecision`].
#[derive(Debug)]
struct Outcome {
    allowed: bool,
    remaining: u64,
    retry_after_secs: u64,
    reset_at: u64,
}

/// A token bucket
/// ([ADR-0003](../../../docs/ADR/0003-rate-limiting.md) "Token Bucket Algorithm").
///
/// Tokens refill continuously at the sustained rate up to the burst capacity
/// and start full, so a fresh bucket admits a burst of `capacity` requests.
#[derive(Debug)]
struct TokenBucket {
    tokens: f64,
    capacity: f64,
    refill_per_sec: f64,
    last_refill: Instant,
}

impl TokenBucket {
    fn of(policy: &EffectiveRateLimit, now: Instant) -> Self {
        let capacity = f64::from(policy.capacity);
        Self {
            tokens: capacity,
            capacity,
            // `rate` is at least 1 and a window at most 86_400 s long, so the
            // division always has a non-zero, finite result.
            refill_per_sec: f64::from(policy.sustained.rate)
                / f64::from(u32::try_from(policy.window_secs()).unwrap_or(1)),
            last_refill: now,
        }
    }

    /// Refills up to `now` and consumes `cost`, reporting what is left.
    ///
    /// A request that cannot pay is not charged: the attempted cost is the most
    /// a rejection may ever consume, and the bucket does not consume even that.
    fn acquire(&mut self, cost: u32, now: Instant) -> (bool, f64) {
        self.refill(now);
        let cost = f64::from(cost);
        if self.tokens < cost {
            return (false, self.tokens);
        }
        self.tokens -= cost;
        (true, self.tokens)
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last_refill);
        self.tokens =
            (self.tokens + elapsed.as_secs_f64() * self.refill_per_sec).min(self.capacity);
        self.last_refill = now;
    }

    /// Seconds until the bucket is full again, from `last_refill`.
    fn seconds_to_full(&self) -> f64 {
        missing_secs(self.capacity - self.tokens, self.refill_per_sec)
    }

    fn outcome(&self, cost: u32, allowed: bool, tokens: f64, unix_now: u64) -> Outcome {
        Outcome {
            allowed,
            remaining: whole_tokens(tokens),
            reset_at: unix_now.saturating_add(whole_seconds(self.seconds_to_full())),
            retry_after_secs: if allowed {
                0
            } else {
                retry_after(f64::from(cost) - tokens, self.refill_per_sec)
            },
        }
    }
}

/// Seconds needed to refill `missing` tokens at `refill_per_sec`.
fn missing_secs(missing: f64, refill_per_sec: f64) -> f64 {
    if missing <= 0.0 || refill_per_sec <= 0.0 {
        return 0.0;
    }
    missing / refill_per_sec
}

/// Seconds to wait before `missing` tokens can be paid again, at least one:
/// a rejected request is never told to retry immediately.
fn retry_after(missing: f64, refill_per_sec: f64) -> u64 {
    whole_seconds(missing_secs(missing, refill_per_sec)).max(1)
}

/// A sliding window: `limit` requests per window, however they are spread.
#[derive(Debug)]
struct SlidingWindow {
    hits: VecDeque<Instant>,
}

impl SlidingWindow {
    /// Records `cost` hits when the window still has room for them.
    fn outcome(
        &mut self,
        window: Duration,
        limit: u64,
        cost: u32,
        now: Instant,
        unix_now: u64,
    ) -> Outcome {
        self.evict(window, now);
        let count = u64::try_from(self.hits.len()).unwrap_or(u64::MAX);
        let remaining = limit.saturating_sub(count);
        let reset_at = unix_now.saturating_add(whole_seconds(
            oldest_or(now, &self.hits)
                .saturating_duration_since(now)
                .as_secs_f64(),
        ));
        if count + u64::from(cost) > limit {
            return Outcome {
                allowed: false,
                remaining,
                // The window frees room as soon as its oldest hit leaves it.
                retry_after_secs: retry_after(
                    (oldest_or(now, &self.hits) + window)
                        .saturating_duration_since(now)
                        .as_secs_f64(),
                    1.0,
                ),
                reset_at,
            };
        }
        for _ in 0..cost {
            self.hits.push_back(now);
        }
        Outcome {
            allowed: true,
            remaining: remaining - u64::from(cost),
            retry_after_secs: 0,
            reset_at,
        }
    }

    /// Drops the hits that aged out of `window`.
    fn evict(&mut self, window: Duration, now: Instant) {
        while self
            .hits
            .front()
            .is_some_and(|oldest| *oldest + window <= now)
        {
            self.hits.pop_front();
        }
    }
}

fn oldest_or(fallback: Instant, hits: &VecDeque<Instant>) -> Instant {
    hits.front().copied().unwrap_or(fallback)
}

/// One counter of the limiter, whichever algorithm the policy selected.
#[derive(Debug)]
enum LimitState {
    Bucket(TokenBucket),
    Window(SlidingWindow),
}

impl LimitState {
    /// Builds the counter `policy` selects, starting full.
    fn of(policy: &EffectiveRateLimit, now: Instant) -> Self {
        match policy.algorithm {
            RateLimitAlgorithm::TokenBucket => Self::Bucket(TokenBucket::of(policy, now)),
            RateLimitAlgorithm::SlidingWindow => Self::Window(SlidingWindow {
                hits: VecDeque::new(),
            }),
        }
    }

    fn outcome(
        &mut self,
        policy: &EffectiveRateLimit,
        limit: u64,
        now: Instant,
        unix_now: u64,
    ) -> Outcome {
        match self {
            Self::Bucket(bucket) => {
                let (allowed, tokens) = bucket.acquire(policy.cost, now);
                bucket.outcome(policy.cost, allowed, tokens, unix_now)
            }
            Self::Window(window) => window.outcome(
                Duration::from_secs(policy.window_secs()),
                limit,
                policy.cost,
                now,
                unix_now,
            ),
        }
    }
}

/// The limit the response headers report: the burst capacity of a token bucket,
/// the sustained rate of a sliding window.
fn limit_of(policy: &EffectiveRateLimit) -> u64 {
    match policy.algorithm {
        RateLimitAlgorithm::TokenBucket => u64::from(policy.capacity),
        RateLimitAlgorithm::SlidingWindow => u64::from(policy.sustained.rate),
    }
}

// ---------------------------------------------------------------------------
// Limiter
// ---------------------------------------------------------------------------

/// The counters of one tenant, keyed by their ADR-0003 key.
#[derive(Debug, Default)]
struct TenantBuckets {
    states: HashMap<String, LimitState>,
}

/// The in-memory rate limiter of the data plane.
///
/// The registry is sharded per tenant: `dashmap` partitions the tenants and the
/// per-tenant mutex makes the refill-and-consume step of one key atomic. No
/// lock is ever held across an `await` — the limiter is synchronous.
#[derive(Debug)]
pub struct RateLimiter {
    clock: Arc<dyn RateLimitClock>,
    tenants: DashMap<Uuid, Mutex<TenantBuckets>>,
}

impl RateLimiter {
    /// Builds a limiter reading `clock`.
    #[must_use]
    pub fn new(clock: Arc<dyn RateLimitClock>) -> Self {
        Self {
            clock,
            tenants: DashMap::new(),
        }
    }

    /// Builds a limiter on the system clock.
    #[must_use]
    pub fn with_system_clock() -> Self {
        Self::new(Arc::new(SystemClock))
    }

    /// Checks `policy` for one request of `identity`, consuming the configured
    /// cost when it admits the request.
    #[must_use]
    pub fn check(
        &self,
        policy: &EffectiveRateLimit,
        resource: &LimitResource,
        identity: &RateLimitIdentity,
    ) -> RateLimitDecision {
        let scope = LimitScope::of(policy.scope);
        let key = rate_limit_key(resource, scope, identity, policy.sustained.window);
        let now = self.clock.now();
        let unix_now = self.clock.unix_now();
        let limit = limit_of(policy);

        let mut tenants = self.tenants.entry(identity.tenant_id).or_default();
        let states = &mut tenants.get_mut().states;
        let outcome = states
            .entry(key.clone())
            .or_insert_with(|| LimitState::of(policy, now))
            .outcome(policy, limit, now, unix_now);

        RateLimitDecision {
            allowed: outcome.allowed,
            key,
            limit,
            remaining: outcome.remaining,
            reset_at: outcome.reset_at,
            retry_after_secs: outcome.retry_after_secs,
            strategy: policy.strategy,
        }
    }

    /// Drops every counter of one resource
    /// ([ADR-0003](../../../docs/ADR/0003-rate-limiting.md) "Redis key
    /// structure": all keys of a resource share its prefix).
    pub fn forget_resource(&self, resource: &LimitResource) {
        let prefix = resource.key_prefix();
        self.tenants.retain(|_, tenant| {
            let mut tenant = tenant.lock();
            tenant.states.retain(|key, _| !key.starts_with(&prefix));
            !tenant.states.is_empty()
        });
    }

    /// Number of live counters, across every tenant (diagnostics only).
    #[must_use]
    pub fn len(&self) -> usize {
        self.tenants
            .iter()
            .map(|entry| entry.value().lock().states.len())
            .sum()
    }

    /// `true` when no counter is live.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The fractional part of a token count is not a whole token and a bucket clamps
/// itself at zero, so the conversion drops neither a sign nor a token.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn whole_tokens(tokens: f64) -> u64 {
    if tokens.is_finite() {
        tokens.floor().max(0.0) as u64
    } else {
        0
    }
}

/// Seconds, rounded up: a client that retries after exactly this long can pay.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn whole_seconds(seconds: f64) -> u64 {
    if seconds.is_finite() {
        seconds.ceil().max(0.0) as u64
    } else {
        u64::MAX
    }
}

/// The outcome of one rate-limit check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitDecision {
    /// Whether the request may proceed.
    pub allowed: bool,
    /// The ADR-0003 key the counter is held under.
    pub key: String,
    /// The merged limit the counter enforces, in requests.
    pub limit: u64,
    /// Requests left before the limit is hit.
    pub remaining: u64,
    /// Unix seconds of the instant the counter resets.
    pub reset_at: u64,
    /// Seconds to wait before retrying, rounded up.
    pub retry_after_secs: u64,
    /// Strategy configured for the limit.
    pub strategy: RateLimitStrategy,
}

impl RateLimitDecision {
    /// The 429 the proxy returns for a rejected request
    /// ([DESIGN.md](../../../docs/DESIGN.md) `cpt-cf-oagw-interface-api`):
    /// `application/problem+json` with `~cf.oagw.rate_limit.exceeded.v1` and the
    /// retry hint in the problem context.
    #[must_use]
    pub fn error(&self) -> OagwError {
        OagwError::RateLimitExceeded {
            retry_after_seconds: self.retry_after_secs,
        }
    }

    /// The headers the response carries: `Retry-After` on a rejection, always,
    /// and the `X-RateLimit-*` set when the policy enables them.
    #[must_use]
    pub fn headers(&self, policy: &EffectiveRateLimit) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if !self.allowed
            && let Ok(retry_after) = HeaderValue::from_str(&self.retry_after_secs.to_string())
        {
            headers.insert(RETRY_AFTER, retry_after);
        }
        if !policy.response_headers {
            return headers;
        }
        for (name, value) in [
            (RATE_LIMIT_LIMIT_HEADER, self.limit.to_string()),
            (RATE_LIMIT_REMAINING_HEADER, self.remaining.to_string()),
            (RATE_LIMIT_RESET_HEADER, self.reset_at.to_string()),
        ] {
            if let (Ok(name), Ok(value)) = (
                HeaderName::from_lowercase(name.as_bytes()),
                HeaderValue::from_str(&value),
            ) {
                headers.insert(name, value);
            }
        }
        headers
    }
}
