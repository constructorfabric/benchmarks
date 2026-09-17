//! Rate limiting: a token bucket per counter key (ADR-0003).
//!
//! # Algorithm
//!
//! Every counter key owns one [`TokenBucket`] with capacity
//! [`RateLimitConfig::effective_burst_capacity`] that refills continuously at
//! `sustained.rate / window_seconds` tokens per second, so a full bucket allows
//! a burst of `capacity` requests and a steady `rate` per window forever
//! (ADR-0003 "1. Algorithm: Token Bucket (default)"). A bucket is created
//! lazily on first use and never expires, and there is **no background
//! sweeper**: a bucket is two `f64`s and an [`Instant`] (~32 bytes), a key
//! exists only while the configuration that produces it exists, and a key that
//! stops being requested stops being written — a sweeper would add a background
//! task and a lock discipline to save a bounded amount of memory that the
//! process already pays for several times over in its connection pool.
//!
//! # The bucket map is bounded
//!
//! "Never expires" is not "unbounded": [`RateLimitLimiter`] holds at most
//! `capacity` buckets (`rate_limit_bucket_capacity` in the gear configuration,
//! [`RateLimitLimiter::DEFAULT_BUCKET_CAPACITY`] by default), because a `scope:
//! ip` counter is keyed on an address the *caller* picks in `X-Forwarded-For`,
//! and one caller minting a fresh address per request would otherwise grow
//! shared memory without limit. When the map is full, a new key first sweeps
//! the **expired** buckets — a bucket whose last update is older than its
//! window has already refilled to full, so dropping it changes no decision —
//! and only then gives the slot of the least recently updated live bucket to
//! the newcomer. The eviction is reported at `debug` level and names the
//! evicted key's **upstream id and scope only**: the identity component of a
//! key is an IP address or a subject id, which is caller data, and caller data
//! does not go into the log.
//!
//! # Effective limit (ADR-0003 "3. Inheritance", DESIGN §3.5 "Shadowing
//! Behavior")
//!
//! [`effective_rate_limit`] walks the tenant chain for the rate limits that
//! apply to a request and returns the **strictest** of them:
//!
//! * the `sharing: enforce` rate limits of the ancestor upstreams that share
//!   the selected upstream's alias — a descendant cannot bypass an enforced
//!   ancestor budget by shadowing the alias (DESIGN §3.5 "Shadowing Behavior");
//! * the selected upstream's own rate limit, whatever its sharing mode;
//! * the matched route's rate limit.
//!
//! The winner supplies **every** parameter of the bucket — capacity, scope,
//! cost and strategy — not only the rate, so a descendant can neither widen the
//! counter (`scope`) nor cheapen a request (`cost`) of a limit an ancestor
//! enforced on it.
//!
//! # Documented deviations
//!
//! * **Rates are compared per second, not per window.** ADR-0003 and DESIGN
//!   §3.5 both write the inheritance as `min(...)`, which is only
//!   dimensionally sound when every limit shares one window. Limits with
//!   different windows are compared as `sustained.rate / window_seconds`, and
//!   the winner's own `rate`/`window` pair is what the response headers report,
//!   so `100/second` loses to `50/minute` (and `X-RateLimit-Limit` then reads
//!   `50/minute`, not `50/second`).
//! * **`inherit` is not clamped.** ADR-0003's inheritance table tightens a
//!   child under an `inherit` parent as well; this slice tightens only
//!   `enforce` ancestors, because a shared `inherit` budget needs the budget
//!   accounting (`budget.mode`, `overcommit_ratio`) the bound wire schema does
//!   not carry. A descendant that inherits a looser limit is therefore not
//!   clamped by it — an ancestor that must bound its descendants configures
//!   `enforce`.
//! * **`sliding_window` behaves as a token bucket.** The algorithm is a
//!   configuration choice of the wire schema; only the token bucket is
//!   implemented (ADR-0003's chosen default). Both enum values take the same
//!   path, so a limit configured as `sliding_window` is enforced at the same
//!   sustained rate and capacity rather than not being enforced at all.
//! * **`queue` and `degrade` behave as `reject`.** ADR-0003 names the MVP as
//!   per-instance `reject`-only rate limiting; a queued or degraded response
//!   has no implementation in this slice, so both strategies reject with 429
//!   rather than silently passing the request through unthrottled.
//! * **`X-RateLimit-*` headers are gateway-owned.** They are stamped on the
//!   gateway's 429 problem response — together with `Retry-After` — and never
//!   forwarded to the upstream, whose response must remain exactly what the
//!   upstream produced (ADR-0007: the gateway does not rewrite an upstream
//!   payload). On an *allowed* request the data plane adds the budget state
//!   (`X-RateLimit-Limit`/`-Remaining`, and `-Reset` only once a request has
//!   been refused, since an unsaturated bucket has no reset to report) after the
//!   upstream answered, so the upstream never sees them either.
//! * **Client IP comes from `X-Forwarded-For` only.** The transport seam the
//!   proxy is served through does not expose the peer socket address, so the
//!   leftmost value of `X-Forwarded-For` is used verbatim (no proxy-chain
//!   parsing) and `None` otherwise. A caller that forges the header only moves
//!   *its own* bucket: a missing value collapses to one shared bucket per
//!   upstream, which is strictly narrower than per-caller buckets and so can
//!   never widen what a limit allows.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use http::HeaderMap;
use uuid::Uuid;

use crate::domain::types::SharingMode;
use crate::domain::types::{RateLimitConfig, RateLimitScope, RateLimitWindow, Route, Upstream};
use crate::error::OagwError;

/// `X-RateLimit-Limit` — the effective sustained rate per window (ADR-0003
/// "More Information").
pub const RATE_LIMIT_LIMIT_HEADER: &str = "x-ratelimit-limit";

/// `X-RateLimit-Remaining` — tokens left in the bucket after the request.
pub const RATE_LIMIT_REMAINING_HEADER: &str = "x-ratelimit-remaining";

/// `X-RateLimit-Reset` — Unix seconds when the bucket can serve again.
pub const RATE_LIMIT_RESET_HEADER: &str = "x-ratelimit-reset";

/// Header value separator of `X-RateLimit-Limit` (`100/minute`).
const LIMIT_SEPARATOR: &str = "/";

/// The word substituted for an absent scope identity in a counter key.
const UNSPECIFIED: &str = "-";

// ---------------------------------------------------------------------------
// Effective limit
// ---------------------------------------------------------------------------

/// The rate limit that applies to a request, once the hierarchy is folded in.
///
/// See the [module documentation](self#effective-limit) for the merge rule.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveRateLimit {
    /// The winning configuration: strictest rate, and the scope, cost and
    /// strategy that come with it.
    pub config: RateLimitConfig,
    /// Which configuration provided it, for the audit trail.
    pub source: RateLimitSource,
}

/// Where the effective rate limit came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitSource {
    /// An enforced ancestor upstream's budget.
    EnforcedAncestor,
    /// The selected upstream's own budget.
    Upstream,
    /// The matched route's budget.
    Route,
}

/// Fold the rate limits that apply to a request into one
/// [`EffectiveRateLimit`].
///
/// `chain` is the ancestor chain, nearest first and including the calling
/// tenant ([`crate::domain::routing::TenantHierarchy::chain`]); `lookup`
/// resolves `(tenant, alias)` in the upstream store. Without a hierarchy the
/// caller passes a chain of the selected upstream's own tenant, which can only
/// ever *narrow* the set of consulted budgets — never widen it.
///
/// `None` when no limit applies: the request is then not throttled at all
/// (there is no implicit default limit, ADR-0003 "Schema Changes" — `rate_limit`
/// is optional).
#[must_use]
pub fn effective_rate_limit<F>(
    chain: &[Uuid],
    selected: &Upstream,
    route: Option<&Route>,
    mut lookup: F,
) -> Option<EffectiveRateLimit>
where
    F: FnMut(Uuid, &str) -> Option<std::sync::Arc<Upstream>>,
{
    // Ancestors are the tenants *above* the one that owns the selected
    // upstream: below it (the calling tenant and its descendants) nobody else
    // can own the alias, because resolution stops at the first owner.
    let ancestors = match chain
        .iter()
        .position(|tenant| *tenant == selected.tenant_id)
    {
        Some(position) => &chain[position + 1..],
        None => chain,
    };

    let mut effective: Option<EffectiveRateLimit> = None;

    // Every enforced ancestor constrains the request, nearest first: an
    // equally tight ancestor keeps the one nearer to the caller, which is the
    // one whose operator made the decision for *this* request.
    for tenant in ancestors {
        let Some(ancestor) = lookup(*tenant, &selected.alias) else {
            continue;
        };
        let Some(limit) = ancestor.spec.rate_limit else {
            continue;
        };
        if limit.sharing != SharingMode::Enforce {
            continue;
        }
        effective = strictest(effective, limit, RateLimitSource::EnforcedAncestor);
    }

    // The selected upstream's own budget, and then the route's, always apply:
    // each is compared against what is already collected, and the strictest
    // wins.
    if let Some(limit) = selected.spec.rate_limit {
        effective = strictest(effective, limit, RateLimitSource::Upstream);
    }
    if let Some(route) = route
        && let Some(limit) = route.spec.rate_limit
    {
        effective = strictest(effective, limit, RateLimitSource::Route);
    }

    effective
}

/// Keep whichever of the two limits is stricter (fewest tokens per second).
///
/// An already-collected limit wins a tie: `effective_rate_limit` feeds the
/// candidates in nearest-first order, so a tie keeps the budget of the resource
/// nearer to the caller.
fn strictest(
    current: Option<EffectiveRateLimit>,
    candidate: RateLimitConfig,
    source: RateLimitSource,
) -> Option<EffectiveRateLimit> {
    match current {
        Some(existing) if tokens_per_second(&existing.config) <= tokens_per_second(&candidate) => {
            Some(existing)
        }
        _ => Some(EffectiveRateLimit {
            config: candidate,
            source,
        }),
    }
}

/// The sustained rate of a limit in tokens per second.
///
/// The comparison unit of [`effective_rate_limit`]: rates declared with
/// different windows are only comparable once they share one.
#[must_use]
pub fn tokens_per_second(config: &RateLimitConfig) -> f64 {
    f64::from(config.sustained.rate) / window_seconds(config.sustained.window) as f64
}

/// Length of a rate-limit window in seconds (ADR-0003 "Schema Changes").
#[must_use]
pub const fn window_seconds(window: RateLimitWindow) -> u64 {
    match window {
        RateLimitWindow::Second => 1,
        RateLimitWindow::Minute => 60,
        RateLimitWindow::Hour => 3_600,
        RateLimitWindow::Day => 86_400,
    }
}

/// `X-RateLimit-Limit` header value: the sustained rate per window.
///
/// Format: `<rate>/<window>`, window spelled as the wire configuration does
/// (`second`, `minute`, `hour`, `day`) — e.g. `100/minute`. The numeric-only
/// alternative of ADR-0003's "More Information" example is ambiguous for the
/// dual-rate configuration, which reports *which* window the number counts.
#[must_use]
pub fn format_limit(config: &RateLimitConfig) -> String {
    let window = match config.sustained.window {
        RateLimitWindow::Second => "second",
        RateLimitWindow::Minute => "minute",
        RateLimitWindow::Hour => "hour",
        RateLimitWindow::Day => "day",
    };
    format!("{}{LIMIT_SEPARATOR}{}", config.sustained.rate, window)
}

// ---------------------------------------------------------------------------
// Counter keys
// ---------------------------------------------------------------------------

/// The identity a rate-limit counter is keyed by.
///
/// Everything in it comes from the request's authenticated context or from
/// configuration, never from a header a caller can pick freely — see the
/// module documentation for the one exception (`X-Forwarded-For`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitRequest {
    /// Upstream the request was resolved to; part of every key, so two
    /// upstreams never share a bucket.
    pub upstream_id: Uuid,
    /// Calling tenant.
    pub tenant_id: Uuid,
    /// Authenticated subject.
    pub subject_id: Uuid,
    /// Matched route, when the request reached one.
    pub route_id: Option<Uuid>,
    /// Client IP, when the request carries `X-Forwarded-For`.
    pub client_ip: Option<IpAddr>,
}

/// The counter key of a request: `<upstream>|<scope>|<scope identity>`.
///
/// Composed with the upstream id first, so that a `global` scope is "one bucket
/// per upstream" rather than one bucket for the whole process: two upstreams
/// with limits of their own must not spend each other's budget (ADR-0003
/// "Distribution", key structure `oagw:ratelimit:{resource_type}:{resource_id}`).
#[must_use]
pub fn counter_key(config: &RateLimitConfig, request: &RateLimitRequest) -> String {
    let (scope, identity) = match config.scope {
        RateLimitScope::Global => ("global", UNSPECIFIED.to_owned()),
        RateLimitScope::Tenant => ("tenant", request.tenant_id.to_string()),
        RateLimitScope::User => ("user", request.subject_id.to_string()),
        RateLimitScope::Ip => (
            "ip",
            request
                .client_ip
                .map_or_else(|| UNSPECIFIED.to_owned(), |ip| ip.to_string()),
        ),
        RateLimitScope::Route => (
            "route",
            request
                .route_id
                .map_or_else(|| UNSPECIFIED.to_owned(), |id| id.to_string()),
        ),
    };

    format!("{}|{scope}|{identity}", request.upstream_id)
}

/// The client IP of a proxy request: the leftmost `X-Forwarded-For` entry.
///
/// The header is read as a single opaque value — the first entry is the client
/// the *first* proxy on the path saw, and parsing the chain (or trusting a
/// later entry) would let a caller choose its own bucket. No header, no
/// address: `None`, and the counter then falls back to one shared bucket.
#[must_use]
pub fn client_ip(headers: &HeaderMap) -> Option<IpAddr> {
    let forwarded = headers.get("x-forwarded-for")?.to_str().ok()?;
    let first = forwarded.split(',').next()?.trim();
    if first.is_empty() {
        return None;
    }
    first.parse().ok()
}

/// Unix seconds of the current instant, for `X-RateLimit-Reset`.
#[must_use]
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        // A clock before the epoch cannot produce a meaningful reset instant;
        // the floor keeps the header a number instead of failing the request.
        .unwrap_or_default()
}

/// The data-plane rate-limit hook (ADR-0003): it owns the limiter.
///
/// The data plane folds the effective limit and hands it over with the request
/// context; this type is the only part that touches the buckets, so the
/// algorithm stays pure and the hook stays a thin adapter.
#[derive(Debug, Clone)]
pub struct RateLimitService {
    limiter: Arc<RateLimitLimiter>,
}

impl RateLimitService {
    /// A hook over `limiter`.
    #[must_use]
    pub fn new(limiter: Arc<RateLimitLimiter>) -> Self {
        Self { limiter }
    }

    /// The limiter the hook drives.
    #[must_use]
    pub const fn limiter(&self) -> &Arc<RateLimitLimiter> {
        &self.limiter
    }
}

impl crate::domain::services::data_plane::RateLimitHook for RateLimitService {
    fn check(
        &self,
        context: &crate::domain::services::data_plane::ProxyContext,
        limit: &EffectiveRateLimit,
    ) -> Result<RateLimitDecision, OagwError> {
        let request = RateLimitRequest {
            upstream_id: context.upstream_id,
            tenant_id: context.tenant_id,
            subject_id: context.subject_id,
            route_id: context.route_id,
            client_ip: context.client_ip,
        };

        Ok(self
            .limiter
            .check(&limit.config, &request, Instant::now(), unix_now()))
    }
}

// ---------------------------------------------------------------------------
// Token bucket
// ---------------------------------------------------------------------------

/// One decision of the rate limiter (ADR-0003 "Response Headers").
///
/// Every field the `429` problem response needs is here: `limit`,
/// `remaining` and `reset` become the `X-RateLimit-*` headers, `retry_after_secs`
/// the `Retry-After` header and the `retry_after_seconds` extension member of
/// the problem document. The data plane stamps them; the limiter only decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitDecision {
    /// Whether the request may proceed.
    pub allowed: bool,
    /// `X-RateLimit-Limit` ([`format_limit`]).
    pub limit: String,
    /// Numeric capacity of the bucket the decision came from;
    /// [`None`](Option) when the caller cannot know it, and then no usage ratio
    /// is reported for it. Carried for the `oagw_rate_limit_usage_ratio` metric
    /// (DESIGN §4.2) and nothing else: the header value is `limit`.
    pub limit_value: Option<u64>,
    /// `X-RateLimit-Remaining`: tokens left in the bucket after the request.
    pub remaining: u64,
    /// `Retry-After` in seconds, rounded up; `0` when the request is allowed,
    /// which carries no `Retry-After` at all (see the module documentation).
    pub retry_after_secs: u64,
    /// `X-RateLimit-Reset`: Unix seconds when the bucket can serve one more
    /// request at the configured cost; `0` when the request is allowed, and the
    /// header is then omitted — a bucket that served the request has no reset
    /// worth reporting.
    pub reset_unix_secs: u64,
}

/// The number of buckets a [`RateLimitLimiter`] holds unless the configuration
/// says otherwise (`rate_limit_bucket_capacity`).
///
/// Large enough that a legitimate deployment never evicts — a bucket is ~32
/// bytes, so the ceiling itself costs about two mebibytes — and small enough
/// that a caller minting `X-Forwarded-For` addresses cannot grow shared memory
/// without bound.
pub const DEFAULT_BUCKET_CAPACITY: usize = 65_536;

/// A thread-safe token bucket per counter key (ADR-0003 "Implementation
/// Notes").
///
/// Buckets are stored in a [`DashMap`] and created on first use; the check
/// itself is a single lock-guarded mutation, so the latency added to a request
/// is one map lookup. The map holds at most `capacity` buckets — see the
/// [module documentation](self#the-bucket-map-is-bounded) for what happens when
/// it is full.
#[derive(Debug)]
pub struct RateLimitLimiter {
    buckets: DashMap<String, TokenBucket>,
    /// The most buckets the map may hold.
    capacity: usize,
}

impl Default for RateLimitLimiter {
    // Manual, so `Default` and `new` agree on the ceiling: a derived `Default`
    // would leave `capacity` at zero, and a limiter that evicts every bucket it
    // creates enforces nothing.
    fn default() -> Self {
        Self::with_capacity(DEFAULT_BUCKET_CAPACITY)
    }
}

impl RateLimitLimiter {
    /// An empty limiter over [`DEFAULT_BUCKET_CAPACITY`] buckets.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty limiter that holds at most `capacity` buckets.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            buckets: DashMap::new(),
            capacity,
        }
    }

    /// The number of live buckets, for tests and operators.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buckets.len()
    }

    /// The most buckets the map holds.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// `true` when no bucket has been created yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }

    /// Take `config.cost` tokens from the bucket of `request`.
    ///
    /// `now` is the monotonic reading the refill is measured against and
    /// `unix_secs` the Unix seconds the reset header is computed from: two
    /// clocks, because a monotonic one cannot be formatted as a date and the
    /// wall clock cannot measure a refill without jumping.
    ///
    /// A rejected request consumes nothing: the caller is told how long to
    /// wait, not charged for the attempt.
    #[must_use]
    pub fn check(
        &self,
        config: &RateLimitConfig,
        request: &RateLimitRequest,
        now: Instant,
        unix_secs: u64,
    ) -> RateLimitDecision {
        let capacity = f64::from(config.effective_burst_capacity());
        let refill_per_sec = tokens_per_second(config);
        let cost = f64::from(config.cost);
        let window = Duration::from_secs(window_seconds(config.sustained.window));

        // Fast path: the bucket already exists (every request after the first
        // for a key). `get_mut` avoids taking the shard's write lock twice.
        let key = counter_key(config, request);
        let mut bucket = match self.buckets.get_mut(key.as_str()) {
            Some(bucket) => bucket,
            // A key seen for the first time costs a bucket, and the map may have
            // to give up another one to pay for it.
            None => {
                self.make_room(&key, window, now);
                self.buckets
                    .entry(key)
                    .or_insert_with(|| TokenBucket::full(capacity, now))
            }
        };

        bucket.refill(capacity, refill_per_sec, now);

        if bucket.tokens >= cost {
            bucket.tokens -= cost;
            return RateLimitDecision {
                allowed: true,
                limit: format_limit(config),
                limit_value: Some(config.effective_burst_capacity().into()),
                remaining: bucket.tokens.floor().max(0.0) as u64,
                retry_after_secs: 0,
                reset_unix_secs: 0,
            };
        }

        // The bucket cannot serve the request yet: report when it will. The
        // tokens stay in the bucket, so a rejected request is never charged.
        let wait = seconds_until(cost - bucket.tokens, refill_per_sec);
        RateLimitDecision {
            allowed: false,
            limit: format_limit(config),
            limit_value: Some(config.effective_burst_capacity().into()),
            remaining: bucket.tokens.floor().max(0.0) as u64,
            retry_after_secs: wait,
            reset_unix_secs: unix_secs.saturating_add(wait),
        }
    }

    /// Free one slot for the bucket of `key` when the map is full.
    ///
    /// Expired buckets go first: one whose last update is older than `window`
    /// has refilled to full anyway, so dropping it cannot change a decision (the
    /// newcomer starts full too). Only when nothing has expired does the least
    /// recently updated live bucket give up its slot, which is the closest
    /// thing to an LRU the bucket's single timestamp allows.
    ///
    /// The map is only ever *at* `capacity` here, never above it, so the ceiling
    /// holds whatever the callers do; a key that already has a bucket (another
    /// thread won the race) makes no room at all.
    fn make_room(&self, key: &str, window: Duration, now: Instant) {
        if self.buckets.len() < self.capacity || self.buckets.contains_key(key) {
            return;
        }

        let expired = |bucket: &TokenBucket| now.duration_since(bucket.updated) >= window;
        if self.evict_while(expired) {
            return;
        }

        // Still full: every bucket is live, so the oldest update is the one to
        // drop. The key is cloned out before the removal, because the iterator
        // holds the shard's lock.
        let oldest = self
            .buckets
            .iter()
            .min_by_key(|entry| entry.value().updated)
            .map(|entry| entry.key().clone());
        if let Some(oldest) = oldest {
            self.buckets.remove(&oldest);
            log_eviction(&oldest, "least_recently_updated");
        }
    }

    /// Remove every bucket `predicate` accepts; `true` when it removed one.
    fn evict_while(&self, predicate: impl Fn(&TokenBucket) -> bool) -> bool {
        let mut removed = false;
        self.buckets.retain(|key, bucket| {
            if predicate(bucket) {
                log_eviction(key, "expired");
                removed = true;
                false
            } else {
                true
            }
        });
        removed
    }
}

/// Report one eviction at `debug` level.
///
/// The record names the evicted key's **upstream id and scope only**. The third
/// component of a counter key — the identity, an IP address or a subject id —
/// is caller data: it identifies who was throttled, and a log is not the place
/// to keep that (the audit record carries the principal a request was
/// authenticated as, which is the only identity worth logging).
fn log_eviction(key: &str, reason: &str) {
    let (upstream_id, scope) = loggable_parts(key);

    tracing::debug!(
        target: "oagw.rate_limit",
        evicted_upstream_id = %upstream_id,
        evicted_scope = %scope,
        reason,
        "evicted a rate-limit bucket to stay within the configured bucket capacity"
    );
}

/// The parts of a counter key an eviction record may name: the upstream id and
/// the scope, never the identity.
///
/// A key that does not have the documented `<upstream>|<scope>|<identity>` shape
/// logs empty fields rather than a guess.
#[must_use]
fn loggable_parts(key: &str) -> (&str, &str) {
    match key.split_once('|') {
        Some((upstream_id, rest)) => (
            upstream_id,
            rest.split_once('|').map_or(rest, |(scope, _)| scope),
        ),
        None => ("", ""),
    }
}

/// Seconds until the bucket recovers `missing` tokens, rounded up.
fn seconds_until(missing: f64, refill_per_sec: f64) -> u64 {
    if missing <= 0.0 {
        return 0;
    }
    // A refill rate of zero is unreachable in practice (`rate` is validated to
    // be at least 1 and every window is at least one second); the ceiling keeps
    // the arithmetic total instead of dividing by zero.
    (missing / refill_per_sec.max(1.0 / 86_400.0)).ceil() as u64
}

/// A token bucket: the tokens it holds and when it was last refilled.
#[derive(Debug)]
struct TokenBucket {
    tokens: f64,
    updated: Instant,
}

impl TokenBucket {
    /// A full bucket: a fresh counter allows a burst of `capacity` requests.
    fn full(capacity: f64, now: Instant) -> Self {
        Self {
            tokens: capacity,
            updated: now,
        }
    }

    /// Add the tokens that accumulated since the last refill and clamp to the
    /// (possibly changed) capacity, so a replaced configuration takes effect on
    /// the very next request.
    fn refill(&mut self, capacity: f64, refill_per_sec: f64, now: Instant) {
        let elapsed = now
            .checked_duration_since(self.updated)
            .unwrap_or(Duration::ZERO);
        self.updated = now;
        let refilled = self.tokens + elapsed.as_secs_f64() * refill_per_sec;
        self.tokens = refilled.min(capacity).max(0.0);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::domain::types::{
        Endpoint, Protocol, RateLimitAlgorithm, RateLimitBurst, RateLimitStrategy,
        RateLimitSustained, RouteMethod, RouteSpec, Scheme, ServerConfig, SharingMode,
        UpstreamSpec,
    };
    use crate::error::OagwErrorKind;

    const UPSTREAM: Uuid = Uuid::from_u128(0x0001);
    const TENANT: Uuid = Uuid::from_u128(0x0002);
    const OTHER_TENANT: Uuid = Uuid::from_u128(0x0003);
    const SUBJECT: Uuid = Uuid::from_u128(0x0004);
    const ROUTE: Uuid = Uuid::from_u128(0x0005);

    fn config(rate: u32, window: RateLimitWindow, capacity: u32) -> RateLimitConfig {
        RateLimitConfig {
            sharing: SharingMode::Private,
            algorithm: RateLimitAlgorithm::TokenBucket,
            sustained: RateLimitSustained { rate, window },
            burst: Some(RateLimitBurst { capacity }),
            scope: RateLimitScope::Tenant,
            strategy: RateLimitStrategy::Reject,
            cost: 1,
        }
    }

    fn request() -> RateLimitRequest {
        RateLimitRequest {
            upstream_id: UPSTREAM,
            tenant_id: TENANT,
            subject_id: SUBJECT,
            route_id: Some(ROUTE),
            client_ip: None,
        }
    }

    fn upstream(alias: &str, tenant: Uuid, rate_limit: Option<RateLimitConfig>) -> Upstream {
        let mut spec = UpstreamSpec {
            alias: Some(alias.to_owned()),
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: Scheme::Http,
                    host: "127.0.0.1".to_owned(),
                    port: 8080,
                }],
            },
            protocol: Protocol::Http,
            rate_limit,
            ..UpstreamSpec::default()
        };
        spec = spec.validate().expect("the upstream spec normalizes");

        Upstream {
            id: UPSTREAM,
            tenant_id: tenant,
            alias: alias.to_owned(),
            created_at: 0,
            updated_at: 0,
            spec,
        }
    }

    fn route(rate_limit: Option<RateLimitConfig>) -> Route {
        Route {
            id: ROUTE,
            tenant_id: TENANT,
            upstream_id: UPSTREAM,
            created_at: 0,
            updated_at: 0,
            spec: RouteSpec {
                upstream_id: UPSTREAM,
                match_rules: crate::domain::types::RouteMatch {
                    http: Some(crate::domain::types::HttpMatch {
                        methods: vec![RouteMethod::Get],
                        path: "/v1".to_owned(),
                        query_allowlist: Vec::new(),
                        path_suffix_mode: crate::domain::types::PathSuffixMode::Append,
                    }),
                    grpc: None,
                },
                enabled: true,
                tags: Vec::new(),
                plugins: None,
                rate_limit,
            },
        }
    }

    #[test]
    fn window_lengths_are_the_documented_ones() {
        assert_eq!(window_seconds(RateLimitWindow::Second), 1);
        assert_eq!(window_seconds(RateLimitWindow::Minute), 60);
        assert_eq!(window_seconds(RateLimitWindow::Hour), 3_600);
        assert_eq!(window_seconds(RateLimitWindow::Day), 86_400);
    }

    #[test]
    fn the_limit_header_names_the_rate_and_the_window() {
        assert_eq!(
            format_limit(&config(100, RateLimitWindow::Minute, 100)),
            "100/minute"
        );
        assert_eq!(
            format_limit(&config(5, RateLimitWindow::Second, 5)),
            "5/second"
        );
        assert_eq!(format_limit(&config(1, RateLimitWindow::Day, 1)), "1/day");
    }

    #[test]
    fn rates_of_different_windows_are_compared_per_second() {
        // 50/minute (0.83/s) is stricter than 100/second, and the winner keeps
        // its own window: a per-window `min` would have reported `50/second`.
        let selected = upstream(
            "api.vendor.com",
            TENANT,
            Some(config(100, RateLimitWindow::Second, 10)),
        );
        let route = route(Some(config(50, RateLimitWindow::Minute, 5)));

        let effective = effective_rate_limit(&[TENANT], &selected, Some(&route), |_, _| None)
            .expect("both limits apply");

        assert_eq!(effective.source, RateLimitSource::Route);
        assert_eq!(format_limit(&effective.config), "50/minute");
        assert_eq!(effective.config.effective_burst_capacity(), 5);
    }

    #[test]
    fn a_route_limit_tighter_than_the_upstreams_wins() {
        let selected = upstream(
            "api.vendor.com",
            TENANT,
            Some(config(100, RateLimitWindow::Minute, 100)),
        );
        let route = route(Some(config(10, RateLimitWindow::Minute, 10)));

        let effective =
            effective_rate_limit(&[TENANT], &selected, Some(&route), |_, _| None).expect("limits");

        assert_eq!(effective.source, RateLimitSource::Route);
        assert_eq!(effective.config.effective_burst_capacity(), 10);
    }

    #[test]
    fn a_looser_route_limit_does_not_widen_the_upstreams() {
        let selected = upstream(
            "api.vendor.com",
            TENANT,
            Some(config(10, RateLimitWindow::Minute, 10)),
        );
        let route = route(Some(config(100, RateLimitWindow::Minute, 100)));

        let effective =
            effective_rate_limit(&[TENANT], &selected, Some(&route), |_, _| None).expect("limits");

        assert_eq!(effective.source, RateLimitSource::Upstream);
        assert_eq!(format_limit(&effective.config), "10/minute");
    }

    #[test]
    fn an_enforced_ancestor_clamps_a_looser_descendant() {
        let ancestor = upstream(
            "api.vendor.com",
            OTHER_TENANT,
            Some(RateLimitConfig {
                sharing: SharingMode::Enforce,
                ..config(5, RateLimitWindow::Minute, 5)
            }),
        );
        let selected = upstream(
            "api.vendor.com",
            TENANT,
            Some(config(100, RateLimitWindow::Minute, 100)),
        );
        let chain = [TENANT, OTHER_TENANT];

        let effective = effective_rate_limit(&chain, &selected, None, |tenant, _| {
            (tenant == OTHER_TENANT).then(|| Arc::new(ancestor.clone()))
        })
        .expect("the enforced ancestor applies");

        assert_eq!(effective.source, RateLimitSource::EnforcedAncestor);
        assert_eq!(format_limit(&effective.config), "5/minute");
    }

    #[test]
    fn an_inherited_or_private_ancestor_does_not_clamp() {
        for sharing in [SharingMode::Inherit, SharingMode::Private] {
            let ancestor = upstream(
                "api.vendor.com",
                TENANT,
                Some(RateLimitConfig {
                    sharing,
                    ..config(5, RateLimitWindow::Minute, 5)
                }),
            );
            let selected = upstream(
                "api.vendor.com",
                TENANT,
                Some(config(100, RateLimitWindow::Minute, 100)),
            );

            let effective =
                effective_rate_limit(&[TENANT, OTHER_TENANT], &selected, None, |tenant, _| {
                    (tenant == OTHER_TENANT).then(|| Arc::new(ancestor.clone()))
                })
                .expect("the upstream's own limit applies");

            assert_eq!(
                effective.source,
                RateLimitSource::Upstream,
                "{sharing:?} ancestors do not constrain a descendant"
            );
            assert_eq!(format_limit(&effective.config), "100/minute");
        }
    }

    #[test]
    fn an_ancestor_without_the_alias_is_not_consulted() {
        let selected = upstream(
            "api.vendor.com",
            TENANT,
            Some(config(100, RateLimitWindow::Minute, 100)),
        );

        let effective = effective_rate_limit(&[TENANT, OTHER_TENANT], &selected, None, |_, _| None)
            .expect("the upstream's own limit applies");

        assert_eq!(effective.source, RateLimitSource::Upstream);
    }

    #[test]
    fn a_hierarchiless_walk_never_widens_the_limit() {
        // `chain` does not contain the selected upstream's tenant: the ancestor
        // walk finds nothing and only the upstream and route limits remain.
        let ancestor = upstream(
            "api.vendor.com",
            OTHER_TENANT,
            Some(RateLimitConfig {
                sharing: SharingMode::Enforce,
                ..config(1, RateLimitWindow::Minute, 1)
            }),
        );
        let selected = upstream(
            "api.vendor.com",
            TENANT,
            Some(config(100, RateLimitWindow::Minute, 100)),
        );

        let effective = effective_rate_limit(&[TENANT], &selected, None, |tenant, _| {
            (tenant == OTHER_TENANT).then(|| Arc::new(ancestor.clone()))
        })
        .expect("the upstream's own limit applies");

        assert_eq!(effective.source, RateLimitSource::Upstream);
    }

    #[test]
    fn no_configuration_means_no_limit() {
        let selected = upstream("api.vendor.com", TENANT, None);
        let route = route(None);

        assert!(
            effective_rate_limit(&[TENANT], &selected, Some(&route), |_, _| None).is_none(),
            "an absent rate_limit config throttles nothing"
        );
    }

    #[test]
    fn every_scope_yields_its_own_key_under_the_same_upstream() {
        let tenant_scoped = config(10, RateLimitWindow::Second, 10);
        let mut global = config(10, RateLimitWindow::Second, 10);
        global.scope = RateLimitScope::Global;
        let mut user = config(10, RateLimitWindow::Second, 10);
        user.scope = RateLimitScope::User;
        let mut ip = config(10, RateLimitWindow::Second, 10);
        ip.scope = RateLimitScope::Ip;
        let mut route_scope = config(10, RateLimitWindow::Second, 10);
        route_scope.scope = RateLimitScope::Route;

        let identity = request();
        let other_tenant = RateLimitRequest {
            tenant_id: OTHER_TENANT,
            ..identity.clone()
        };
        let other_subject = RateLimitRequest {
            subject_id: OTHER_TENANT,
            ..identity.clone()
        };
        let other_ip = RateLimitRequest {
            client_ip: Some("203.0.113.7".parse().expect("an IP")),
            ..identity.clone()
        };

        let tenant_key = counter_key(&tenant_scoped, &identity);
        assert_eq!(
            tenant_key,
            counter_key(&tenant_scoped, &identity.clone()),
            "the same tenant reads the same bucket"
        );
        assert_ne!(
            tenant_key,
            counter_key(&tenant_scoped, &other_tenant),
            "scope=tenant is per caller tenant"
        );
        assert_eq!(
            tenant_key,
            counter_key(&tenant_scoped, &other_subject),
            "scope=tenant does not key by who the caller is"
        );
        assert_ne!(
            tenant_key,
            counter_key(&user, &identity),
            "the scope name is part of the key"
        );
        assert_ne!(
            counter_key(&user, &identity),
            counter_key(&user, &other_subject),
            "scope=user is per subject"
        );
        assert_eq!(
            counter_key(&global, &identity),
            counter_key(&global, &other_tenant),
            "scope=global is one bucket per upstream"
        );
        assert_ne!(
            counter_key(&ip, &identity),
            counter_key(&ip, &other_ip),
            "scope=ip separates client addresses"
        );
        assert_ne!(
            counter_key(&route_scope, &identity),
            counter_key(
                &route_scope,
                &RateLimitRequest {
                    route_id: None,
                    ..identity.clone()
                }
            ),
            "scope=route keys by route id"
        );
        assert_eq!(
            counter_key(&route_scope, &identity),
            counter_key(&route_scope, &other_subject),
            "scope=route ignores who the caller is"
        );
    }

    #[test]
    fn two_upstreams_never_share_a_bucket() {
        let limit = config(10, RateLimitWindow::Second, 10);
        let identity = request();

        assert_ne!(
            counter_key(&limit, &identity),
            counter_key(
                &limit,
                &RateLimitRequest {
                    upstream_id: OTHER_TENANT,
                    ..identity.clone()
                }
            ),
            "the upstream id prefixes every key"
        );
    }

    #[test]
    fn an_absent_client_ip_shares_one_bucket_per_upstream() {
        let mut limit = config(10, RateLimitWindow::Second, 10);
        limit.scope = RateLimitScope::Ip;
        let identity = request();

        assert_eq!(
            counter_key(&limit, &identity),
            counter_key(
                &limit,
                &RateLimitRequest {
                    upstream_id: UPSTREAM,
                    tenant_id: OTHER_TENANT,
                    subject_id: OTHER_TENANT,
                    route_id: None,
                    client_ip: None,
                }
            ),
            "no address means no per-caller separation"
        );
    }

    #[test]
    fn the_leftmost_forwarded_for_entry_is_the_client_ip() {
        let mut headers = HeaderMap::new();
        assert_eq!(client_ip(&headers), None, "no header, no address");

        headers.insert(
            "x-forwarded-for",
            "203.0.113.7, 10.0.0.1".parse().expect("a header value"),
        );
        assert_eq!(
            client_ip(&headers),
            Some("203.0.113.7".parse().expect("an IP")),
            "the leftmost entry wins, the chain is not parsed"
        );

        headers.insert(
            "x-forwarded-for",
            "not-an-address".parse().expect("a value"),
        );
        assert_eq!(
            client_ip(&headers),
            None,
            "an unparseable value is not an address"
        );
    }

    #[test]
    fn a_burst_up_to_the_capacity_is_allowed_then_rejected() {
        let limiter = RateLimitLimiter::new();
        // Capacity 3, one token per second: three requests at once, no fourth.
        let limit = config(1, RateLimitWindow::Second, 3);
        let identity = request();
        let start = Instant::now();

        for _ in 0..3 {
            assert!(
                limiter.check(&limit, &identity, start, 1_000).allowed,
                "a full bucket allows the burst"
            );
        }

        let rejection = limiter.check(&limit, &identity, start, 1_000);
        assert!(!rejection.allowed, "the fourth request is rejected");
        assert_eq!(rejection.remaining, 0);
        assert_eq!(rejection.retry_after_secs, 1);
        assert_eq!(rejection.reset_unix_secs, 1_001);
    }

    #[test]
    fn the_bucket_refills_continuously() {
        let limiter = RateLimitLimiter::new();
        let limit = config(2, RateLimitWindow::Second, 1);
        let identity = request();
        let start = Instant::now();

        assert!(limiter.check(&limit, &identity, start, 1_000).allowed);
        assert!(
            !limiter.check(&limit, &identity, start, 1_000).allowed,
            "a capacity of 1 allows one request at a time"
        );

        // Half a second at 2 tokens/second refills exactly one token.
        let later = start + Duration::from_millis(500);
        assert!(
            limiter.check(&limit, &identity, later, 1_000).allowed,
            "the refill is continuous, not per window"
        );
        assert!(
            !limiter.check(&limit, &identity, later, 1_000).allowed,
            "and no faster than the configured rate"
        );
    }

    #[test]
    fn a_burst_never_exceeds_the_capacity_even_after_a_long_idle() {
        let limiter = RateLimitLimiter::new();
        // 2 tokens/second but a capacity of 3: an idle hour refills to 3, not 7200.
        let limit = config(2, RateLimitWindow::Second, 3);
        let identity = request();
        let start = Instant::now();

        assert!(limiter.check(&limit, &identity, start, 1_000).allowed);
        let after_an_hour = start + Duration::from_secs(3_600);
        for _ in 0..3 {
            assert!(
                limiter
                    .check(&limit, &identity, after_an_hour, 1_000)
                    .allowed,
                "the idle hour refilled to the capacity of 3, not to 7200 tokens"
            );
        }
        assert!(
            !limiter
                .check(&limit, &identity, after_an_hour, 1_000)
                .allowed,
            "the capacity bounds the burst whatever the idle time was"
        );
    }

    #[test]
    fn a_cost_above_one_consumes_proportionally_more() {
        let limiter = RateLimitLimiter::new();
        let mut limit = config(10, RateLimitWindow::Minute, 10);
        limit.cost = 4;
        let identity = request();
        let start = Instant::now();

        assert!(limiter.check(&limit, &identity, start, 1_000).allowed);
        assert!(limiter.check(&limit, &identity, start, 1_000).allowed);
        let rejection = limiter.check(&limit, &identity, start, 1_000);
        assert!(
            !rejection.allowed,
            "2 x 4 tokens leaves 2, the third request needs 4"
        );
        assert_eq!(rejection.remaining, 2, "only what is left is reported");
    }

    #[test]
    fn a_rejection_reports_consistent_headers() {
        let limiter = RateLimitLimiter::new();
        let mut limit = config(1, RateLimitWindow::Minute, 2);
        limit.cost = 2;
        let identity = request();
        let start = Instant::now();
        let unix = 1_766_000_000;

        assert!(limiter.check(&limit, &identity, start, unix).allowed);
        let rejection = limiter.check(&limit, &identity, start, unix);
        assert!(!rejection.allowed, "the bucket is empty");

        assert_eq!(rejection.limit, "1/minute");
        assert_eq!(rejection.remaining, 0);
        assert_eq!(
            rejection.reset_unix_secs - unix,
            rejection.retry_after_secs,
            "Reset and Retry-After describe the same wait"
        );
        assert!(
            rejection.retry_after_secs >= 1,
            "the wait is at least one second"
        );
    }

    #[test]
    fn a_rejected_request_consumes_nothing() {
        let limiter = RateLimitLimiter::new();
        let limit = config(1, RateLimitWindow::Minute, 1);
        let identity = request();
        let start = Instant::now();

        assert!(limiter.check(&limit, &identity, start, 1_000).allowed);
        let first = limiter.check(&limit, &identity, start, 1_000);
        let second = limiter.check(&limit, &identity, start, 1_000);
        assert!(!first.allowed && !second.allowed, "both are rejected");

        assert_eq!(first, second, "the bucket did not move");
        assert_eq!(limiter.len(), 1, "one bucket for the key");
    }

    #[test]
    fn tenants_do_not_share_a_bucket() {
        let limiter = RateLimitLimiter::new();
        let limit = config(1, RateLimitWindow::Minute, 1);
        let identity = request();
        let other = RateLimitRequest {
            tenant_id: OTHER_TENANT,
            ..identity.clone()
        };
        let start = Instant::now();

        assert!(limiter.check(&limit, &identity, start, 1_000).allowed);
        assert!(
            limiter.check(&limit, &other, start, 1_000).allowed,
            "scope=tenant gives every tenant its own bucket"
        );
        assert_eq!(limiter.len(), 2);
    }

    #[test]
    fn a_limiter_at_capacity_keeps_the_ceiling_and_still_decides() {
        // `scope: ip` is the scope that makes a ceiling necessary: the identity
        // is an address the caller picked in `X-Forwarded-For`, and one caller
        // minting a fresh one per request would otherwise grow shared memory
        // without limit.
        let limiter = RateLimitLimiter::with_capacity(2);
        let mut limit = config(10, RateLimitWindow::Minute, 10);
        limit.scope = RateLimitScope::Ip;
        let start = Instant::now();

        for address in ["203.0.113.7", "203.0.113.8", "203.0.113.9"] {
            let caller = RateLimitRequest {
                client_ip: Some(address.parse().expect("an IP")),
                ..request()
            };
            let decision = limiter.check(&limit, &caller, start, 1_000);
            assert!(
                decision.allowed,
                "a bucket being evicted never fails the request it had to make room for: {address}"
            );
        }

        assert_eq!(
            limiter.len(),
            2,
            "the ceiling holds: two addresses, two buckets"
        );
        assert_eq!(limiter.capacity(), 2);
    }

    #[test]
    fn an_expired_bucket_is_swept_before_a_live_one() {
        let limiter = RateLimitLimiter::with_capacity(2);
        // Capacity 1, one token per second: a spent bucket is empty, and one
        // that has not been touched for a window is full again, so dropping it
        // cannot change a decision.
        let limit = config(1, RateLimitWindow::Second, 1);
        let expired = request();
        let live = RateLimitRequest {
            tenant_id: OTHER_TENANT,
            ..expired.clone()
        };
        let newcomer = RateLimitRequest {
            tenant_id: SUBJECT,
            ..expired.clone()
        };
        let start = Instant::now();

        assert!(limiter.check(&limit, &expired, start, 1_000).allowed);
        let later = start + Duration::from_secs(10);
        assert!(limiter.check(&limit, &live, later, 1_010).allowed);
        assert_eq!(limiter.len(), 2, "both fit under the ceiling");

        // The map is full: the first bucket is ten seconds old, the second was
        // updated just now. The newcomer is admitted and the ceiling still holds.
        assert!(limiter.check(&limit, &newcomer, later, 1_010).allowed);
        assert_eq!(limiter.len(), 2);

        // The live bucket survived with its spend: it is empty, so the next
        // request for that identity is refused. Had the eviction taken the
        // live bucket instead, a fresh full bucket would have admitted it. This
        // is the invariant the ceiling exists to keep — no *live* budget is
        // thrown away while a bucket that has already refilled to full can give
        // up its slot instead.
        assert!(
            !limiter.check(&limit, &live, later, 1_010).allowed,
            "a live bucket is preferred for eviction over an expired one"
        );
    }

    #[test]
    fn a_map_under_the_capacity_never_evicts() {
        let limiter = RateLimitLimiter::with_capacity(4);
        let limit = config(1, RateLimitWindow::Minute, 1);
        let start = Instant::now();

        for tenant in [TENANT, OTHER_TENANT, SUBJECT] {
            let identity = RateLimitRequest {
                tenant_id: tenant,
                ..request()
            };
            assert!(limiter.check(&limit, &identity, start, 1_000).allowed);
        }
        assert_eq!(limiter.len(), 3, "nothing was evicted");

        // The first bucket still holds its spend: it was never a candidate.
        let first = RateLimitRequest {
            tenant_id: TENANT,
            ..request()
        };
        assert!(
            !limiter.check(&limit, &first, start, 1_000).allowed,
            "a bucket is only evicted when the map is full"
        );
    }

    #[test]
    fn an_eviction_is_logged_without_the_identity_component() {
        // The identity of a `scope: ip` key is an address the caller sent, so it
        // is caller data: the eviction record names the upstream and the scope
        // and stops there.
        let mut limit = config(1, RateLimitWindow::Minute, 1);
        limit.scope = RateLimitScope::Ip;
        let caller = RateLimitRequest {
            client_ip: Some("203.0.113.7".parse().expect("an IP")),
            ..request()
        };
        let key = counter_key(&limit, &caller);

        let (upstream_id, scope) = loggable_parts(&key);
        assert_eq!(upstream_id, UPSTREAM.to_string());
        assert_eq!(scope, "ip", "the scope is configuration, not caller data");
        let record = format!("{upstream_id}|{scope}");
        assert!(
            !record.contains("203.0.113.7"),
            "the address the caller sent never reaches the log: {record}"
        );
    }

    #[test]
    fn a_strategy_of_queue_or_degrade_still_rejects() {
        // Documented deviation: only `reject` has a behaviour in this slice.
        for strategy in [RateLimitStrategy::Queue, RateLimitStrategy::Degrade] {
            let limiter = RateLimitLimiter::new();
            let mut limit = config(1, RateLimitWindow::Minute, 1);
            limit.strategy = strategy;
            let identity = request();
            let start = Instant::now();

            assert!(limiter.check(&limit, &identity, start, 1_000).allowed);
            assert!(
                !limiter.check(&limit, &identity, start, 1_000).allowed,
                "{strategy:?} has no implementation and falls back to reject"
            );
        }
    }

    #[test]
    fn a_sliding_window_limit_is_enforced_as_a_token_bucket() {
        // Documented deviation: the algorithm enum is a configuration choice,
        // only the token bucket is implemented.
        let limiter = RateLimitLimiter::new();
        let mut limit = config(1, RateLimitWindow::Minute, 1);
        limit.algorithm = RateLimitAlgorithm::SlidingWindow;
        let identity = request();
        let start = Instant::now();

        assert!(limiter.check(&limit, &identity, start, 1_000).allowed);
        assert!(
            !limiter.check(&limit, &identity, start, 1_000).allowed,
            "the limit is enforced as a token bucket, not ignored"
        );
    }

    #[test]
    fn a_cost_above_the_capacity_can_never_be_served() {
        let limiter = RateLimitLimiter::new();
        let mut limit = config(1, RateLimitWindow::Minute, 2);
        limit.cost = 5;
        let identity = request();
        let start = Instant::now();

        let rejection = limiter.check(&limit, &identity, start, 1_000);
        assert!(
            !rejection.allowed,
            "the bucket can never hold 5 tokens at once"
        );
        assert_eq!(rejection.remaining, 2, "the two tokens are still there");
        assert_eq!(
            rejection.retry_after_secs, 180,
            "three more tokens at one a minute, however long the caller waits"
        );
        assert_eq!(rejection.reset_unix_secs, 1_000 + 180);
    }

    #[test]
    fn the_kind_of_a_rate_limit_rejection_is_the_documented_one() {
        let error = crate::error::OagwError::rate_limit_exceeded("over budget", 7);

        assert_eq!(error.kind(), OagwErrorKind::RateLimitExceeded);
        assert_eq!(error.status().as_u16(), 429);
        assert_eq!(error.retry_after_secs(), Some(7));
    }
}
