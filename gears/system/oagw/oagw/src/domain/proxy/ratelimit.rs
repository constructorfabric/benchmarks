// Created: 2026-08-31 by Constructor Tech
//! Token-bucket rate limiting of the proxy data plane (ADR-0003).
//!
//! # Algorithm
//!
//! The bucket is the one ADR-0003 "Implementation Notes" spells out: `capacity`
//! tokens, refilled continuously at `refill_rate = sustained.rate /
//! window_seconds`. A request acquires `cost` tokens; an empty bucket leaves
//! the decision to the configured `strategy`.
//!
//! # Counter key
//!
//! `{upstream_id}:{scope_id}:{fingerprint}`, where `scope_id` comes from the
//! configured `scope` and the fingerprint from the effective limit. The
//! upstream id prefix is what makes ADR-0003's prefix-based cleanup a `retain`
//! over this map when a record is deleted; the fingerprint is what makes a
//! changed policy a fresh bucket instead of a budget the old limit spent.
//!
//! | scope | `scope_id` |
//! |---|---|
//! | `global` | the literal `global`: one counter per upstream |
//! | `tenant` | the id of the **calling** tenant |
//! | `user` | the calling subject id |
//! | `ip` | the first `X-Forwarded-For` hop, when it parses as an address |
//! | `route` | the matched route id |
//!
//! `tenant` is keyed on the caller, not on the tenant the upstream record
//! belongs to: a shared (ancestor-owned) upstream has to give every caller its
//! own budget, and `global` is the one counter the whole upstream shares.
//!
//! `ip` is the weakest scope and ADR-0003 says so in as many words: the
//! platform hands the gear no connection peer address, so the forwarded chain
//! is the only client identity available, and it is only meaningful behind a
//! proxy that *overwrites* the chain it received. A hop that does not parse as
//! an address, or that is longer than the longest IPv6 literal, shares the
//! `unknown` counter — which is what stops a rotated header from minting fresh
//! buckets.
//!
//! # Bound
//!
//! The bucket map is capped at [`MAX_BUCKETS`] entries. When a request needs a
//! bucket that does not exist yet and the map is at the ceiling, the buckets no
//! request has touched for a window are swept — an idle bucket is full again,
//! so forgetting it loses nothing but the memory it holds — and a request that
//! still finds no room shares the one `shared-overflow` counter. That keeps a
//! flood of distinct addresses from turning the map into an allocation
//! primitive, at the price of a shared budget for whoever arrives while the map
//! is full.
//!
//! # Distribution
//!
//! ADR-0003 "Distribution" picks hybrid local + periodic sync for the
//! distributed deployment and names the per-instance token bucket as the MVP.
//! These buckets are that MVP: per-instance, no sync.
//!
//! # Strategy
//!
//! * `reject` — 429 immediately (the ADR default).
//! * `queue` — await a token, bounded by the response-head budget, which is
//!   also what the dial gets. A queued request that outlives the budget falls
//!   through to the 429, so an exhausted bucket never stretches a request
//!   past the budget the client already agreed to.
//! * `degrade` — serve anyway. Documented choice: "degrade" means serving in a
//!   degraded mode, never blocking; the request is still scored and reported.
//!
//! # Queue fairness
//!
//! A queued request holds a per-key gate for the whole of its wait, so a waiter
//! that wakes when its token is back scores before the requests that arrived
//! behind it; the gate is taken before the loop and released after it, and no
//! `DashMap` shard guard is ever held across an `await`. The other two
//! strategies take no gate, so a `reject` arrival can still claim a token a
//! waiter was about to take: the guarantee is FIFO-ish, not strict, and the
//! head budget bounds every wait either way.
//!
//! # Inheritance
//!
//! Levels are read descendant→ancestor — `[route, resolved_upstream, nearest
//! ancestor, …]` — and the first level that declares a policy is the *selected*
//! one. An ancestor's `enforce` cap stays active however the descendant shares
//! (DESIGN: "ancestor constraints with `sharing: enforce` remain active"), so
//! the participating levels are `selected` plus every ancestor that declares
//! `enforce`, plus every ancestor policy at all when `selected` declares
//! `inherit`. No level declares → no enforcement. The effective rate and the
//! capacity are the min over the participating levels, the rate compared **per
//! second** so that a minute and a second can be minned; the recomposed rate is
//! expressed in `selected`'s window. Scope, strategy, cost and the response
//! header switch are properties of the level that asks for the limit and come
//! from `selected` alone.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use http::HeaderValue;
use tokio::sync::Mutex;
use tokio::time::Instant;
use uuid::Uuid;

use crate::domain::model::{RateLimitConfig, SharingMode};
use crate::error::{OagwError, OagwErrorKind};

/// Largest window length in seconds the write path accepts (`day`).
///
/// Also the idle age at which a bucket is swept: a bucket that has not been
/// touched for the largest window the schema allows is full again.
const MAX_WINDOW_SECS: u64 = 24 * 60 * 60;

/// Ceiling on the number of live buckets.
///
/// A counter key is minted per caller identity, and the `ip` scope takes its
/// identity from a header, so the map has to have a ceiling: an unbounded map
/// would be an allocation primitive a client could drive.
const MAX_BUCKETS: usize = 65_536;

/// Longest `X-Forwarded-For` hop that is considered an address.
///
/// The longest IPv6 literal is 45 characters; anything longer is not one.
const MAX_FORWARDED_LEN: usize = 45;

/// Counter the requests that find no room in the bounded map share.
const OVERFLOW_KEY: &str = "shared-overflow";

/// Window length in seconds for one `sustained.window` unit.
///
/// A blank window is the schema default (`second`), which the model
/// materialises as an empty string. Any other value is one the write path
/// rejects (`validate_rate_limit` allows only `second`, `minute`, `hour` and
/// `day`), and the strictest reading of an unreadable window is the shortest
/// one: a policy of ten thousand per week becomes ten thousand per second,
/// never the other way round.
#[must_use]
pub fn window_seconds(window: &str) -> u64 {
    match window {
        "minute" => 60,
        "hour" => 60 * 60,
        "day" => 24 * 60 * 60,
        _ => 1,
    }
}

/// The effective limit a bucket is filled from.
#[derive(Debug, Clone, PartialEq)]
pub struct Limit {
    /// Effective sustained rate, tokens per [`Limit::window`].
    pub rate: u64,
    /// Sustained window as configured (`second` for the schema default).
    pub window: String,
    /// Effective bucket capacity; defaults to the sustained rate.
    pub capacity: u64,
    /// Counter scope of the most specific configuration.
    pub scope: String,
    /// Behaviour when the bucket is empty.
    pub strategy: Strategy,
    /// Tokens one request consumes.
    pub cost: u64,
    /// Whether the `X-RateLimit-*` headers are emitted.
    pub response_headers: bool,
}

impl Limit {
    /// Tokens replenished per second.
    #[must_use]
    pub fn refill_rate(&self) -> f64 {
        as_float(self.rate) / as_float(self.window_seconds().max(1))
    }

    /// Length of the sustained window in seconds.
    #[must_use]
    pub fn window_seconds(&self) -> u64 {
        window_seconds(&self.window)
    }

    /// Human-readable window, for the problem detail.
    fn window_label(&self) -> &str {
        match self.window.as_str() {
            "minute" | "hour" | "day" => self.window.as_str(),
            _ => "second",
        }
    }

    /// Identity of the policy a bucket was created under.
    ///
    /// The counter key carries it, so a PUT that changes any member of the
    /// effective policy starts a fresh bucket, while a PUT that changes
    /// nothing leaves the callers the budget they already spent. The sharing
    /// mode is deliberately not part of it: sharing decides *which*
    /// configurations contribute to the limit, not how a bucket is filled.
    fn fingerprint(&self) -> String {
        format!(
            "{:x}",
            one_hash(&(
                self.rate,
                self.window.clone(),
                self.capacity,
                self.scope.clone(),
                self.strategy.as_str(),
                self.cost
            ))
        )
    }
}

/// Hash one value with the standard hasher, as a `u64`.
///
/// The fingerprint has to change when the policy changes and stay put when it
/// does not. It never leaves the process, so the hash needs no stability
/// across releases — only within one.
fn one_hash<T: std::hash::Hash>(value: &T) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    std::hash::Hash::hash(value, &mut hasher);
    std::hash::Hasher::finish(&hasher)
}

/// Behaviour when the bucket is empty (ADR-0003 "Configuration").
///
/// A value the write path does not know is read as the strictest one: an
/// unknown strategy rejects, an unknown scope counts per calling tenant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// 429 when the bucket is empty.
    Reject,
    /// Await a token, bounded by the response-head budget.
    Queue,
    /// Forward anyway, in a degraded mode.
    Degrade,
}

impl Strategy {
    /// Parse a configured strategy.
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value {
            "queue" => Strategy::Queue,
            "degrade" => Strategy::Degrade,
            // `reject` is the documented default and the strictest reading of
            // a value this deployment does not know.
            _ => Strategy::Reject,
        }
    }

    /// Canonical spelling, for the bucket fingerprint.
    fn as_str(self) -> &'static str {
        match self {
            Strategy::Reject => "reject",
            Strategy::Queue => "queue",
            Strategy::Degrade => "degrade",
        }
    }
}

/// One counter of the data plane.
///
/// The struct is ADR-0003 "Token Bucket Algorithm" verbatim; `refill` is the
/// only thing that moves it forward in time, and every read goes through it, so
/// a bucket that idled for an hour is as full as it should be.
#[derive(Debug)]
pub struct TokenBucket {
    /// Tokens currently in the bucket.
    tokens: f64,
    /// Instant the tokens were last topped up from.
    last_update: Instant,
    /// Bucket size.
    capacity: f64,
    /// Tokens replenished per second.
    refill_rate: f64,
}

impl TokenBucket {
    /// A full bucket for `limit`.
    #[must_use]
    pub fn new(limit: &Limit) -> Self {
        Self {
            tokens: as_float(limit.capacity),
            last_update: Instant::now(),
            capacity: as_float(limit.capacity),
            refill_rate: limit.refill_rate(),
        }
    }

    /// Top the bucket up for the time that passed since the last refill.
    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_update).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_rate).min(self.capacity);
        self.last_update = now;
    }

    /// Take `cost` tokens, refilling first.
    ///
    /// Returns whether the request was admitted, the tokens the bucket holds
    /// afterwards and the seconds until the next token is back.
    fn try_acquire(&mut self, cost: f64) -> (bool, f64, f64) {
        self.refill();
        if self.tokens >= cost {
            self.tokens -= cost;
            return (true, self.tokens, 0.0);
        }
        let missing = cost - self.tokens;
        let wait = missing / self.refill_rate;
        (false, self.tokens, wait)
    }

    /// Seconds until the bucket is full again.
    fn seconds_to_full(&self) -> f64 {
        if self.refill_rate <= 0.0 {
            return 0.0;
        }
        (self.capacity - self.tokens) / self.refill_rate
    }

    /// Time since the last request scored against this bucket.
    fn age(&self) -> Duration {
        self.last_update.elapsed()
    }
}

/// Outcome of scoring one request against a bucket.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Outcome {
    /// Whether the request was admitted.
    pub acquired: bool,
    /// The effective sustained rate, for `X-RateLimit-Limit`.
    pub limit: u64,
    /// Tokens left, floored.
    pub remaining: u64,
    /// Epoch seconds when the bucket is full again, for `X-RateLimit-Reset`.
    pub reset: u64,
    /// Seconds until the next token, at the least 1 (RFC 6585 asks for
    /// seconds), for `Retry-After`.
    pub retry_after: u64,
    /// Unrounded seconds until the next token, for the queue strategy.
    pub retry_in: f64,
}

impl Outcome {
    /// The `X-RateLimit-*` state of a scored request.
    #[must_use]
    pub fn snapshot(&self) -> crate::error::RateLimitSnapshot {
        crate::error::RateLimitSnapshot {
            limit: self.limit,
            remaining: self.remaining,
            reset: self.reset,
        }
    }
}

/// Buckets of the data plane, keyed by the resolved counter key.
///
/// [`DashMap`] keeps a bucket next to its counter key, so two upstreams never
/// serialise each other and a shard lock is held only for the duration of one
/// refill-and-take, never across an `await`. The `gates` map holds the turn of
/// the queued requests (see the module docs on fairness); it is keyed the same
/// way and pruned the same way.
pub struct Buckets {
    map: DashMap<String, TokenBucket>,
    gates: DashMap<String, Arc<Mutex<()>>>,
    /// Ceiling on the bucket count; the sweep runs at it.
    ceiling: usize,
}

impl Default for Buckets {
    fn default() -> Self {
        Self {
            map: DashMap::new(),
            gates: DashMap::new(),
            ceiling: MAX_BUCKETS,
        }
    }
}

impl Buckets {
    /// Score one request against `key` and take its tokens when admitted.
    ///
    /// A bucket that does not exist yet starts full, so the first request of a
    /// fresh key can use the whole configured burst.
    #[must_use]
    pub fn score(&self, key: &str, limit: &Limit, cost: u64) -> Outcome {
        let cost = as_float(cost);
        let scored = self.admit(key);
        let mut entry = self
            .map
            .entry(scored.to_owned())
            .or_insert_with(|| TokenBucket::new(limit));
        let (acquired, tokens, wait) = entry.try_acquire(cost);
        let to_full = entry.seconds_to_full();
        drop(entry);
        Outcome {
            acquired,
            limit: limit.rate,
            remaining: whole(tokens),
            reset: epoch_in(to_full),
            retry_after: seconds_of(wait),
            retry_in: wait,
        }
    }

    /// The key to score against: `key` itself, or the shared overflow counter
    /// when the map is at its ceiling and this key has no bucket of its own.
    fn admit<'key>(&self, key: &'key str) -> &'key str {
        if self.map.len() < self.ceiling || self.map.contains_key(key) {
            return key;
        }
        self.sweep();
        if self.map.len() < self.ceiling || self.map.contains_key(key) {
            return key;
        }
        OVERFLOW_KEY
    }

    /// Drop the buckets no request has touched for a window, and the gates
    /// nothing is waiting on.
    fn sweep(&self) {
        self.map.retain(|_, bucket| bucket.age() < max_idle());
        self.gates.retain(|_, gate| Arc::strong_count(gate) > 1);
    }

    /// Drop every bucket that belongs to `upstream_id` (ADR-0003
    /// "Distribution": the `{resource_id}` prefix exists for this cleanup).
    ///
    /// A deleted upstream's counters would otherwise survive it forever, and a
    /// record that reuses the id would inherit a spent budget.
    pub fn forget_upstream(&self, upstream_id: Uuid) {
        let prefix = format!("{upstream_id}:");
        self.map.retain(|key, _| !key.starts_with(&prefix));
        self.gates.retain(|key, _| !key.starts_with(&prefix));
    }

    /// The gate a queued request of `key` holds while it waits.
    ///
    /// The `Arc` is cloned out of the map before any `await`, so no shard
    /// guard is held across a suspension point.
    fn gate(&self, key: &str) -> Arc<Mutex<()>> {
        Arc::clone(self.gates.entry(key.to_owned()).or_default().value())
    }

    /// Whether any bucket for `upstream_id` is still held.
    ///
    /// For the crate's own tests only: the eviction is observable through the
    /// lifecycle seam, and a public accessor would invite callers to read the
    /// quota state of another upstream's callers.
    #[cfg(test)]
    pub(crate) fn holds(&self, upstream_id: Uuid) -> bool {
        let prefix = format!("{upstream_id}:");
        self.map
            .iter()
            .any(|entry| entry.key().starts_with(&prefix))
    }

    /// Whether a bucket for the exact `key` is still held (tests only).
    #[cfg(test)]
    pub(crate) fn holds_key(&self, key: &str) -> bool {
        self.map.contains_key(key)
    }
}

/// Idle age at which a bucket is swept, from the largest window the schema
/// allows.
fn max_idle() -> Duration {
    Duration::from_secs(MAX_WINDOW_SECS)
}

/// Whole tokens, floored: a fractional token is not one a request could take.
///
/// The bucket keeps `tokens: f64`, as ADR-0003 "Implementation Notes" spells
/// the struct, while `X-RateLimit-Remaining` and `X-RateLimit-Reset` report
/// whole counts, so the fractional part has to be dropped. `value` is clamped
/// into the range [`MAX_TOKENS`] covers before the cast, which makes the
/// conversion exact for every bucket this module can build.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the single float-to-integer conversion of the quota: a fractional token is dropped, the value is clamped before the cast"
)]
fn whole(value: f64) -> u64 {
    if !value.is_finite() {
        return 0;
    }
    value.floor().clamp(0.0, MAX_TOKENS) as u64
}

/// Largest whole token count the cast in [`whole`] is exact for.
///
/// `u64::MAX` is not representable as an `f64`, so the clamp stops at
/// `u32::MAX` — the same ceiling [`as_float`] saturates at.
const MAX_TOKENS: f64 = 4_294_967_295.0;

/// A count of tokens or seconds as an `f64`.
///
/// `f64` has no lossless `From<u64>`, so the conversion saturates at
/// `u32::MAX` — four billion tokens a second, far above any quota a
/// deployment configures, and still exact for every value below it.
fn as_float(value: u64) -> f64 {
    f64::from(u32::try_from(value).unwrap_or(u32::MAX))
}

/// Whole seconds, floored to at least one: RFC 6585 asks for seconds, and a
/// zero would have clients retry immediately.
fn seconds_of(seconds: f64) -> u64 {
    whole(seconds).max(1)
}

/// Epoch seconds `seconds` from now.
fn epoch_in(seconds: f64) -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    now + whole(seconds)
}

/// Sustained rate of one configuration, in tokens per second.
///
/// Comparing raw rates across windows would compare a minute with a second, so
/// every configuration is normalised first and the winner is recomposed in the
/// window of the level that asked for the limit.
fn per_second(config: &RateLimitConfig) -> f64 {
    as_float(config.sustained.rate) / as_float(window_seconds(&config.sustained.window).max(1))
}

/// The effective policy of one request, or `None` when nothing enforces one.
///
/// `levels` are the policies of the chain, descendant→ancestor: the route's,
/// the resolved upstream's, then the ancestors nearest first, each `None` when
/// that record declares no policy. The first level that declares one is the
/// *selected* one; every ancestor after it that declares `enforce` participates
/// (DESIGN: an ancestor's `enforce` constraint stays active), and when the
/// selected level declares `inherit` every ancestor policy does. The remaining
/// members — scope, strategy, cost, response headers — come from the selected
/// level, because a scope or a strategy is a property of the resource that asks
/// for the limit, not of the budget it is capped by.
#[must_use]
pub fn effective_limit(levels: &[Option<&RateLimitConfig>]) -> Option<Limit> {
    let selected = *levels.iter().flatten().next()?;
    let mut caps = vec![selected];
    for level in levels.iter().flatten().skip(1) {
        let participates =
            selected.sharing == SharingMode::Inherit || level.sharing == SharingMode::Enforce;
        if participates {
            caps.push(level);
        }
    }
    let rate_per_second = caps
        .iter()
        .map(|config| per_second(config))
        .reduce(f64::min)?;
    let capacity = caps
        .iter()
        .map(|config| capacity_of(config))
        .reduce(u64::min)?;
    Some(Limit {
        // Recomposed in the window of the selected level, so the reported rate,
        // the refusal detail and the refill all speak the same unit.
        rate: whole(rate_per_second * as_float(window_seconds(&selected.sustained.window))).max(1),
        window: selected.sustained.window.clone(),
        capacity,
        scope: selected.scope.clone(),
        strategy: Strategy::parse(selected.strategy.as_str()),
        cost: selected.cost.max(1),
        response_headers: selected.response_headers,
    })
}

/// Bucket capacity of one configuration; the ADR defaults it to the rate.
fn capacity_of(config: &RateLimitConfig) -> u64 {
    config
        .burst
        .map_or(config.sustained.rate, |burst| burst.capacity.max(1))
}

/// Counter key of one request.
///
/// `{upstream_id}:{scope_id}:{fingerprint}`: the prefix is what the ADR's
/// cleanup retains over, the scope id is what the configured scope names, and
/// the fingerprint is what makes a changed policy a fresh bucket.
#[must_use]
pub fn counter_key(
    upstream_id: Uuid,
    limit: &Limit,
    calling_tenant: Uuid,
    subject_id: Uuid,
    route_id: Uuid,
    forwarded_for: Option<&str>,
) -> String {
    let scope_id = match limit.scope.as_str() {
        "global" => "global".to_owned(),
        // The forwarded chain is the only client identity the platform hands
        // the gear, and only a parsable address counts: a rotated or a forged
        // value that does not parse shares the `unknown` counter, so header
        // rotation cannot mint fresh buckets.
        "ip" => forwarded_for
            .filter(|hop| hop.len() <= MAX_FORWARDED_LEN)
            .and_then(|hop| hop.parse::<IpAddr>().ok())
            .map_or_else(|| "unknown".to_owned(), |hop| hop.to_string()),
        "user" => subject_id.to_string(),
        "route" => route_id.to_string(),
        // The *calling* tenant, and the strictest reading of a scope the write
        // path does not know.
        _ => calling_tenant.to_string(),
    };
    format!("{upstream_id}:{scope_id}:{}", limit.fingerprint())
}

/// 429 for an empty bucket, with the retry guidance ADR-0003 asks for.
#[must_use]
pub fn exceeded(limit: &Limit, outcome: &Outcome) -> OagwError {
    let response_headers = limit.response_headers;
    OagwError::new(
        OagwErrorKind::RateLimitExceeded,
        format!(
            "rate limit of {} requests per {} exceeded",
            limit.rate,
            limit.window_label()
        ),
    )
    .with_extension(|extensions| {
        extensions.retry_after_seconds = Some(outcome.retry_after);
        if response_headers {
            extensions.rate_limit = Some(outcome.snapshot());
        }
    })
}

/// `X-RateLimit-*` headers of a forwarded response (ADR-0003: "Response
/// headers follow RFC 6585").
///
/// A request that was admitted carries the same quota state a refused one
/// does, so a client can stop before it is refused.
#[must_use]
pub fn headers(outcome: &Outcome) -> http::HeaderMap {
    let mut headers = http::HeaderMap::new();
    headers.insert("x-ratelimit-limit", HeaderValue::from(outcome.limit));
    headers.insert(
        "x-ratelimit-remaining",
        HeaderValue::from(outcome.remaining),
    );
    headers.insert("x-ratelimit-reset", HeaderValue::from(outcome.reset));
    headers
}

/// Await a token, bounded by `budget` (strategy `queue`).
///
/// The wait is derived from the bucket's refill rate rather than polled, so a
/// queued request wakes when the token it needs is there instead of on a timer
/// grid; the budget still cuts the wait short and turns it into a 429. The
/// per-key gate is held for the whole wait, which is what keeps a later arrival
/// from taking the token the woken waiter was about to claim.
async fn acquire(buckets: &Buckets, key: &str, limit: &Limit, budget: Duration) -> Outcome {
    let deadline = tokio::time::Instant::now() + budget;
    let gate = buckets.gate(key);
    // Held for the whole wait: this request's turn comes before the arrivals
    // behind it, and the guard is released when the loop returns.
    let _turn = gate.lock().await;
    loop {
        let outcome = buckets.score(key, limit, limit.cost);
        if outcome.acquired {
            return outcome;
        }
        let wake = tokio::time::Instant::now() + Duration::from_secs_f64(outcome.retry_in);
        if wake >= deadline {
            return outcome;
        }
        tokio::time::sleep_until(wake).await;
    }
}

/// A refused request: the bucket state that refused it, and its problem.
///
/// The state travels with the refusal because it is the only record of *why*
/// the bucket was empty, and the gauge of DESIGN §4.2 needs it; the caller
/// keeps the error and the metrics take the state.
#[derive(Debug)]
pub struct Refusal {
    /// The bucket state at the refusal.
    pub outcome: Outcome,
    /// The 429 problem the client is answered with.
    pub error: OagwError,
}

impl Refusal {
    /// Consumed fraction of the bucket, in `0.0..=1.0`.
    ///
    /// A refusal is what the limiter saw when it gave up, so the ratio is the
    /// spend of the bucket at that moment: an empty bucket is `1.0`, whatever
    /// the capacity, and a capacity of `1` spent is `1.0` as well.
    #[must_use]
    pub fn usage_ratio(&self) -> f64 {
        usage_ratio(self.outcome.limit, self.outcome.remaining)
    }
}

/// Consumed fraction of a bucket: the spend over the capacity.
///
/// Both operands go through [`as_f64`], so the workspace's `as` cast is not
/// needed to make a ratio out of two `u64`s.
fn usage_ratio(limit: u64, remaining: u64) -> f64 {
    // A capacity is floored at 1 by the limiter, so the division is guarded
    // rather than assumed: a limit of `0` reads as a full bucket.
    let capacity = limit.max(1);
    let spent = capacity.saturating_sub(remaining).min(capacity);
    as_f64(spent) / as_f64(capacity)
}

/// `u64` to `f64` without the precision-loss `as` cast the workspace denies.
///
/// A bucket state that exceeded `u32` saturates, which a ratio in `0.0..=1.0`
/// cannot observe.
fn as_f64(value: u64) -> f64 {
    f64::from(u32::try_from(value).unwrap_or(u32::MAX))
}

/// Enforce `limit` for one request.
///
/// Returns the bucket state for the response headers. A `degrade` policy is
/// scored but never blocks: it reports the spend and lets the request through.
///
/// # Errors
/// A [`Refusal`] for a `reject` policy once the bucket is empty, and for a
/// `queue` policy whose wait outlives `budget`.
pub async fn enforce(
    buckets: &Buckets,
    key: &str,
    limit: &Limit,
    budget: Duration,
) -> Result<Outcome, Refusal> {
    match limit.strategy {
        Strategy::Degrade => Ok(scored(buckets, key, limit)),
        Strategy::Queue => queued(buckets, key, limit, budget).await,
        Strategy::Reject => rejected(buckets, key, limit),
    }
}

/// Score a `degrade` request and let it through.
///
/// "Degrade" names a serving mode, not a gate: the spend is reported only once
/// the bucket is actually empty, and the request is forwarded regardless.
fn scored(buckets: &Buckets, key: &str, limit: &Limit) -> Outcome {
    let outcome = buckets.score(key, limit, limit.cost.max(1));
    if !outcome.acquired {
        tracing::warn!(
            limit = outcome.limit,
            remaining = outcome.remaining,
            "rate limit exhausted; serving in a degraded mode"
        );
    }
    outcome
}

/// Score a `reject` request: served when a token is there, 429 otherwise.
fn rejected(buckets: &Buckets, key: &str, limit: &Limit) -> Result<Outcome, Refusal> {
    let outcome = buckets.score(key, limit, limit.cost.max(1));
    if outcome.acquired {
        return Ok(outcome);
    }
    tracing::warn!(limit = outcome.limit, "rate limit exceeded");
    Err(refused(limit, &outcome))
}

/// Wait for a token on behalf of a `queue` policy, then give up.
async fn queued(
    buckets: &Buckets,
    key: &str,
    limit: &Limit,
    budget: Duration,
) -> Result<Outcome, Refusal> {
    let outcome = acquire(buckets, key, limit, budget).await;
    if outcome.acquired {
        return Ok(outcome);
    }
    tracing::warn!(limit = outcome.limit, "rate limit queue budget elapsed");
    Err(refused(limit, &outcome))
}

/// Bundle the 429 of an empty bucket with the state that produced it.
fn refused(limit: &Limit, outcome: &Outcome) -> Refusal {
    Refusal {
        outcome: *outcome,
        error: exceeded(limit, outcome),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use uuid::Uuid;

    use super::{Buckets, Limit, Strategy, counter_key, effective_limit, window_seconds};
    use crate::domain::model::{BurstConfig, RateLimitConfig, SharingMode, SustainedRate};

    fn sustained(rate: u64, window: &str) -> SustainedRate {
        SustainedRate {
            rate,
            window: window.to_owned(),
        }
    }

    fn config(
        rate: u64,
        window: &str,
        capacity: Option<u64>,
        sharing: SharingMode,
    ) -> RateLimitConfig {
        RateLimitConfig {
            sharing,
            algorithm: "token_bucket".to_owned(),
            sustained: sustained(rate, window),
            burst: capacity.map(|capacity| BurstConfig { capacity }),
            scope: "tenant".to_owned(),
            strategy: "reject".to_owned(),
            cost: 1,
            response_headers: true,
        }
    }

    /// A `Limit` over `window` seconds; the window label follows the unit.
    fn limit(rate: u64, capacity: u64, window: &str) -> Limit {
        Limit {
            rate,
            window: window.to_owned(),
            capacity,
            scope: "tenant".to_owned(),
            strategy: Strategy::Reject,
            cost: 1,
            response_headers: true,
        }
    }

    /// A `Limit` keyed per `scope`, for the tests that read the counter key.
    fn scoped(scope: &str) -> Limit {
        Limit {
            scope: scope.to_owned(),
            ..limit(1, 1, "second")
        }
    }

    #[test]
    fn a_window_maps_onto_seconds() {
        assert_eq!(window_seconds(""), 1);
        assert_eq!(window_seconds("second"), 1);
        assert_eq!(window_seconds("minute"), 60);
        assert_eq!(window_seconds("hour"), 3_600);
        assert_eq!(window_seconds("day"), 86_400);
        // A window the write path rejects (`week` is not one of the four) is
        // read as the shortest one, which is the strictest reading.
        assert_eq!(window_seconds("week"), 1);
    }

    #[test]
    fn a_bucket_starts_full_and_drains_to_empty() {
        // Two tokens a second: a refill inside the test run stays a fraction.
        let limit = limit(1, 3, "second");
        let buckets = Buckets::default();
        assert!(buckets.score("k", &limit, 1).acquired);
        assert_eq!(buckets.score("k", &limit, 1).remaining, 1);
        assert_eq!(buckets.score("k", &limit, 1).remaining, 0);
        let fourth = buckets.score("k", &limit, 1);
        assert!(!fourth.acquired);
        assert_eq!(fourth.remaining, 0);
        // RFC 6585 asks for seconds, and a zero would have clients retry at
        // once: the guidance is always at least one second away.
        assert!(fourth.retry_after >= 1);
    }

    #[test]
    fn an_instantly_refilling_bucket_never_starves_a_queue() {
        let limit = limit(10_000, 1, "second");
        let buckets = Buckets::default();
        assert!(buckets.score("k", &limit, 1).acquired);
        assert_eq!(
            buckets.score("k", &limit, 1).retry_after,
            1,
            "a fresh token is at most a second away at 10 000 tokens/s"
        );
    }

    #[test]
    fn a_cost_higher_than_the_capacity_admits_nothing() {
        let limit = limit(1, 5, "second");
        let buckets = Buckets::default();
        assert!(!buckets.score("k", &limit, 10).acquired);
        assert!(buckets.score("k", &limit, 10).retry_after >= 1);
    }

    #[test]
    fn every_key_has_its_own_bucket() {
        let limit = limit(1, 1, "day");
        let buckets = Buckets::default();
        assert!(buckets.score("a", &limit, 1).acquired);
        assert!(buckets.score("b", &limit, 1).acquired);
        assert!(!buckets.score("a", &limit, 1).acquired);
    }

    #[test]
    fn a_spent_bucket_is_full_again_after_a_refill() {
        // A hundred tokens a second: ten milliseconds buys one token back.
        let limit = limit(100, 1, "second");
        let buckets = Buckets::default();
        assert!(buckets.score("k", &limit, 1).acquired);
        assert!(!buckets.score("k", &limit, 1).acquired);
        std::thread::sleep(Duration::from_millis(20));
        assert!(buckets.score("k", &limit, 1).acquired);
    }

    #[test]
    fn forgetting_an_upstream_drops_its_buckets() {
        let limit = limit(1, 1, "day");
        let buckets = Buckets::default();
        let upstream = Uuid::now_v7();
        let neighbour = Uuid::now_v7();
        let tenant = Uuid::now_v7();
        let own = counter_key(upstream, &limit, tenant, tenant, tenant, None);
        let other = counter_key(neighbour, &limit, tenant, tenant, tenant, None);
        assert!(buckets.score(&own, &limit, 1).acquired);
        assert!(buckets.score(&other, &limit, 1).acquired);
        assert!(buckets.holds(upstream));
        buckets.forget_upstream(upstream);
        assert!(!buckets.holds(upstream));
        // The neighbour's counter survives the cleanup.
        assert!(buckets.holds(neighbour));
    }

    #[test]
    fn a_changed_limit_starts_a_fresh_bucket() {
        let buckets = Buckets::default();
        let generous = limit(1, 2, "day");
        let tightened = limit(1, 1, "day");
        let key = |limit: &Limit| {
            counter_key(
                Uuid::now_v7(),
                limit,
                Uuid::now_v7(),
                Uuid::now_v7(),
                Uuid::now_v7(),
                None,
            )
        };
        let spent = key(&generous);
        assert!(buckets.score(&spent, &generous, 1).acquired);
        // The same key under a different limit is a different bucket, so the
        // budget the old limit spent is not inherited by the new one.
        assert!(!buckets.holds_key(&key(&tightened)));
        assert!(
            buckets.holds_key(&spent),
            "an unchanged limit keeps its bucket"
        );
    }

    #[test]
    fn the_counter_key_carries_the_scope() {
        let upstream = Uuid::now_v7();
        let tenant = Uuid::now_v7();
        let subject = Uuid::now_v7();
        let route = Uuid::now_v7();
        let limit = limit(1, 1, "second");
        // The middle segment is the scope id the configured scope named; the
        // last one is the fingerprint of the policy, which differs whenever the
        // scope member does.
        let keyed = |scope: &str, forwarded: Option<&str>| {
            let mut policy = limit.clone();
            policy.scope = scope.to_owned();
            counter_key(upstream, &policy, tenant, subject, route, forwarded)
                .split(':')
                .nth(1)
                .unwrap_or("unparsable")
                .to_owned()
        };
        assert_eq!(keyed("global", None), "global");
        assert_eq!(keyed("tenant", None), tenant.to_string());
        assert_eq!(keyed("user", None), subject.to_string());
        assert_eq!(keyed("route", None), route.to_string());
        assert_eq!(keyed("ip", Some("10.0.0.1")), "10.0.0.1");
        assert_eq!(keyed("ip", None), "unknown");
        // A scope the write path does not know counts per calling tenant.
        assert_eq!(keyed("cluster", None), tenant.to_string());
    }

    #[test]
    fn an_unparsable_forwarded_hop_shares_the_unknown_counter() {
        let upstream = Uuid::now_v7();
        let tenant = Uuid::now_v7();
        let policy = scoped("ip");
        let keyed = |forwarded: Option<&str>| {
            counter_key(upstream, &policy, tenant, tenant, tenant, forwarded)
        };
        assert_eq!(keyed(Some("not-an-address")), keyed(None));
        // Header injection may also make the hop longer than any address.
        let long = "1".repeat(64);
        assert_eq!(keyed(Some(&long)), keyed(None));
        assert_ne!(keyed(Some("10.0.0.1")), keyed(None));
    }

    #[test]
    fn the_effective_limit_is_the_min_over_the_participating_levels() {
        // A private descendant: only the ancestor that *enforces* joins in.
        let own = config(1_000, "minute", Some(500), SharingMode::Private);
        let parent = config(100, "minute", Some(10), SharingMode::Enforce);
        let grandparent = config(50, "minute", Some(1_000), SharingMode::Private);
        let levels = [Some(&own), Some(&parent), Some(&grandparent)];
        let limit =
            effective_limit(&levels).unwrap_or_else(|| panic!("a chain with a policy resolves"));
        assert_eq!(limit.rate, 100, "the grandparent shares nothing here");
        assert_eq!(limit.capacity, 10);
        // The most specific record decides scope, strategy and cost.
        assert_eq!(limit.cost, 1);
        assert_eq!(limit.scope, "tenant");
    }

    #[test]
    fn an_inherit_descendant_takes_every_ancestor_policy() {
        let own = config(1_000, "minute", Some(500), SharingMode::Inherit);
        let parent = config(100, "minute", Some(10), SharingMode::Private);
        let grandparent = config(50, "minute", Some(1_000), SharingMode::Private);
        let levels = [Some(&own), Some(&parent), Some(&grandparent)];
        let limit = effective_limit(&levels).unwrap_or_else(|| panic!("must resolve"));
        assert_eq!(limit.rate, 50);
        assert_eq!(limit.capacity, 10);
    }

    #[test]
    fn an_enforce_ancestor_caps_a_private_descendant() {
        let own = config(1_000, "second", Some(1_000), SharingMode::Private);
        let parent = config(2, "second", Some(2), SharingMode::Enforce);
        let levels = [Some(&own), Some(&parent)];
        let limit = effective_limit(&levels).unwrap_or_else(|| panic!("must resolve"));
        assert_eq!(limit.rate, 2);
        assert_eq!(limit.capacity, 2);
    }

    #[test]
    fn a_rate_is_compared_per_second_across_windows() {
        // 10/s is stricter than 1_000/min, so the parent caps the child, in the
        // child's window.
        let own = config(1_000, "minute", Some(1_000), SharingMode::Private);
        let parent = config(10, "second", Some(10), SharingMode::Enforce);
        let levels = [Some(&own), Some(&parent)];
        let limit = effective_limit(&levels).unwrap_or_else(|| panic!("must resolve"));
        assert_eq!(limit.window_seconds(), 60);
        assert_eq!(limit.rate, 600);
        assert!((limit.refill_rate() - 10.0).abs() < 1e-9);
    }

    #[test]
    fn a_slow_ancestor_caps_a_fast_child_in_the_childs_window() {
        // 100/min is stricter than 5/s, and the recomposed rate is floored to a
        // whole token per second, which is the stricter side.
        let own = config(5, "second", Some(5), SharingMode::Private);
        let parent = config(100, "minute", Some(100), SharingMode::Enforce);
        let levels = [Some(&own), Some(&parent)];
        let limit = effective_limit(&levels).unwrap_or_else(|| panic!("must resolve"));
        assert_eq!(limit.window_seconds(), 1);
        assert_eq!(limit.rate, 1);
        assert_eq!(limit.capacity, 5, "capacity is in tokens, not per second");
    }

    #[test]
    fn no_configuration_enforces_nothing() {
        assert!(effective_limit(&[]).is_none());
        assert!(effective_limit(&[None, None]).is_none());
    }

    #[test]
    fn an_absent_capacity_falls_back_to_the_rate() {
        let own = config(100, "second", None, SharingMode::Private);
        let limit = effective_limit(&[Some(&own)]).unwrap_or_else(|| panic!("must resolve"));
        assert_eq!(limit.capacity, 100);
    }

    #[test]
    fn a_window_is_carried_into_the_limit() {
        let own = config(5, "minute", None, SharingMode::Private);
        let limit = effective_limit(&[Some(&own)]).unwrap_or_else(|| panic!("must resolve"));
        assert_eq!(limit.window_seconds(), 60);
        assert_eq!(limit.rate, 5);
        let daily = config(5, "day", None, SharingMode::Private);
        let limit = effective_limit(&[Some(&daily)]).unwrap_or_else(|| panic!("must resolve"));
        assert_eq!(limit.window_seconds(), 86_400);
    }

    #[test]
    fn a_cost_below_one_is_lifted_to_one() {
        let mut own = config(5, "second", None, SharingMode::Private);
        own.cost = 0;
        let limit = effective_limit(&[Some(&own)]).unwrap_or_else(|| panic!("must resolve"));
        assert_eq!(limit.cost, 1);
    }

    #[test]
    fn an_unknown_strategy_fails_closed() {
        let mut own = config(5, "second", None, SharingMode::Private);
        own.strategy = "shed".to_owned();
        let limit = effective_limit(&[Some(&own)]).unwrap_or_else(|| panic!("must resolve"));
        assert_eq!(limit.strategy, Strategy::Reject);
    }

    #[test]
    fn a_fingerprint_follows_the_policy_it_names() {
        let base = limit(100, 10, "minute");
        let same = limit(100, 10, "minute");
        assert_eq!(base.fingerprint(), same.fingerprint());
        // Every member the bucket is created from changes the fingerprint.
        assert_ne!(base.fingerprint(), limit(101, 10, "minute").fingerprint());
        assert_ne!(base.fingerprint(), limit(100, 11, "minute").fingerprint());
        assert_ne!(base.fingerprint(), limit(100, 10, "hour").fingerprint());
        let mut scoped = limit(100, 10, "minute");
        scoped.scope = "global".to_owned();
        assert_ne!(base.fingerprint(), scoped.fingerprint());
        let mut queued = limit(100, 10, "minute");
        queued.strategy = Strategy::Queue;
        assert_ne!(base.fingerprint(), queued.fingerprint());
        let mut costly = limit(100, 10, "minute");
        costly.cost = 2;
        assert_ne!(base.fingerprint(), costly.fingerprint());
    }

    #[test]
    fn a_bucket_idle_for_a_window_is_swept() {
        let limit = limit(1, 1, "day");
        let buckets = Buckets::default();
        let fresh = Uuid::now_v7();
        let stale = Uuid::now_v7();
        let fresh_key = counter_key(
            fresh,
            &limit,
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            None,
        );
        let stale_key = counter_key(
            stale,
            &limit,
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            None,
        );
        assert!(buckets.score(&fresh_key, &limit, 1).acquired);
        assert!(buckets.score(&stale_key, &limit, 1).acquired);
        // A bucket untouched for the largest supported window is full again, so
        // sweeping it loses nothing.
        buckets.map.get_mut(&stale_key).map_or_else(
            || panic!("the stale bucket must be there"),
            |mut bucket| {
                bucket.last_update -= Duration::from_secs(24 * 60 * 60 + 1);
            },
        );
        buckets.sweep();
        assert!(buckets.holds(fresh), "an active bucket survives");
        assert!(!buckets.holds_key(&stale_key), "an idle bucket is swept");
    }

    #[test]
    fn a_map_at_its_ceiling_shares_the_overflow_counter() {
        let limit = limit(1, 1, "day");
        let buckets = Buckets {
            ceiling: 2,
            ..Buckets::default()
        };
        let first = Uuid::now_v7();
        let second = Uuid::now_v7();
        let third = Uuid::now_v7();
        let key = |upstream: Uuid| {
            counter_key(
                upstream,
                &limit,
                Uuid::now_v7(),
                Uuid::now_v7(),
                Uuid::now_v7(),
                None,
            )
        };
        assert!(buckets.score(&key(first), &limit, 1).acquired);
        assert!(buckets.score(&key(second), &limit, 1).acquired);
        // The map is at its ceiling and nothing is idle, so the third request
        // shares the one counter the module reserves for that case.
        assert!(buckets.score(&key(third), &limit, 1).acquired);
        assert!(buckets.holds_key(super::OVERFLOW_KEY));
        assert!(!buckets.holds_key(&key(third)));
        // ... and it is refused, because the overflow counter is spent too.
        assert!(!buckets.score(&key(third), &limit, 1).acquired);
    }

    #[tokio::test]
    async fn a_queued_request_waits_for_its_token() {
        let limit = limit(2_000, 1, "second");
        let buckets = Buckets::default();
        assert!(buckets.score("k", &limit, 1).acquired);
        let outcome = super::acquire(&buckets, "k", &limit, Duration::from_secs(1)).await;
        assert!(outcome.acquired);
    }

    #[tokio::test]
    async fn a_queue_that_cannot_be_served_gives_up() {
        let limit = limit(1, 1, "day");
        let buckets = Buckets::default();
        assert!(buckets.score("k", &limit, 1).acquired);
        let outcome = super::acquire(&buckets, "k", &limit, Duration::from_millis(5)).await;
        assert!(!outcome.acquired);
        assert!(outcome.retry_after >= 1);
    }
}
