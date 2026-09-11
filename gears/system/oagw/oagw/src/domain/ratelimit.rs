//! The pure decision core of the rate-limiting feature
//! (`cpt-cf-oagw-feature-rate-limiting`).
//!
//! The module consumes the effective limit
//! `cpt-cf-oagw-algo-effective-merge` handed over as a value and produces the
//! two outcomes a caller can observe: an admitted request carrying the
//! `X-RateLimit-*` headers and a refusal carrying `Retry-After`. It enforces
//! and never computes a limit: no `min()`, no tenant-chain walk and no sharing
//! mode is evaluated here, and the merged block the limit came from is
//! consumed without being re-validated, `cpt-cf-oagw-algo-shape-validation`
//! having already bound its field set at persist time.
//!
//! The module is pure in the sense the proxy path needs it to be: no I/O, no
//! store, no network, no background task and no clock other than the monotonic
//! one, so a wall-clock adjustment cannot inject tokens and every delay is a
//! function of the bucket state and of the instant the caller injects. The
//! registry that owns the buckets is the data-plane type of
//! `crate::infra::proxy::limiter`, which is the only caller that mutates one.
//!
//! [`EffectiveRateLimit`] carries the enforcement parameters, [`TokenBucket`]
//! the reference bucket of ADR 0003, [`decide`] the per-check decision,
//! [`resolve_scope_key`] the counter key and [`RateLimitOutcome`] the
//! caller-observable result.

use std::net::IpAddr;
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::domain::error::OagwError;
use crate::domain::model::{
    ALGORITHM_SLIDING_WINDOW, ALGORITHM_TOKEN_BUCKET, DEFAULT_RATE_COST, RateLimitConfig, Route,
    SCOPE_GLOBAL, SCOPE_IP, SCOPE_TENANT, SCOPE_USER, STRATEGY_REJECT, WINDOW_DAY, WINDOW_HOUR,
    WINDOW_MINUTE, WINDOW_SECOND,
};

// @cpt-begin:cpt-cf-oagw-dod-dual-rate-config:p1:inst-full

/// The first component of every counter key, the key structure
/// `cpt-cf-oagw-adr-rate-limiting` states for its Redis backend. The trailing
/// date-bucket component of that structure is dropped: it belongs to the
/// deferred sync protocol and a lazily refilled bucket has no wall-clock
/// window to anchor it to.
pub const KEY_PREFIX: &str = "oagw:ratelimit:";

/// The `{resource_type}` of a counter key resolved at the matched route, used
/// when the effective block is the route's own.
pub const RESOURCE_TYPE_ROUTE: &str = "route";

/// The `{resource_type}` of a counter key resolved at the selected upstream,
/// used for every block the merge took from an upstream tier.
pub const RESOURCE_TYPE_UPSTREAM: &str = "upstream";

/// `X-RateLimit-Limit`, the numeric sustained rate of the effective limit.
pub const HEADER_LIMIT: &str = "X-RateLimit-Limit";

/// `X-RateLimit-Remaining`, the tokens the bucket holds after the decision.
pub const HEADER_REMAINING: &str = "X-RateLimit-Remaining";

/// `X-RateLimit-Reset`, the seconds until the bucket refills to capacity.
pub const HEADER_RESET: &str = "X-RateLimit-Reset";

/// `Retry-After`, the delay a refused request is told to wait for.
pub const HEADER_RETRY_AFTER: &str = "Retry-After";

// @cpt-begin:cpt-cf-oagw-algo-token-bucket:p1:inst-tb-01
// The four fields of the reference struct of ADR 0003's implementation note —
// `tokens`, `last_update`, `capacity` and `refill_rate` — carried 1:1, with the
// `cost` of a request coming from the merged block and the ADR default `1`
// when the validated payload omits it.
// @cpt-end:cpt-cf-oagw-algo-token-bucket:p1:inst-tb-01
/// The reference bucket of `cpt-cf-oagw-adr-rate-limiting`'s implementation
/// note, 1:1, plus the one field the fixed-window approximation of FEATURE
/// §1.5 needs: the aligned window boundary it grants at.
///
/// A bucket is never shared between two counter keys and never persisted, so
/// the struct carries no identity of its own.
#[derive(Debug, Clone, PartialEq)]
pub struct TokenBucket {
    /// The balance the bucket holds, never negative and never above
    /// `capacity`.
    pub tokens: f64,
    /// The instant of the last access, the anchor of the lazy refill.
    pub last_update: Instant,
    /// The maximum the bucket holds, `burst.capacity` or the ADR default.
    pub capacity: f64,
    /// Tokens per second, derived from the effective limit value.
    pub refill_rate: f64,
    /// The aligned window boundary of the fixed-window approximation, the
    /// entry's own first boundary being the instant the bucket was created.
    pub window_start: Instant,
}

/// The enforcement parameters of the effective limit
/// (`cpt-cf-oagw-dod-dual-rate-config`).
///
/// The limit value itself is the `min()` the merge computed and is consumed
/// here as a number: the sustained rate and its window drive the refill, the
/// burst capacity and the cost drive the acquisition, and the remaining fields
/// are the single-valued parameters of the merged block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveRateLimit {
    /// The algorithm of the merged block: `token_bucket` exact,
    /// `sliding_window` approximated.
    pub algorithm: String,
    /// The numeric core of the limit, tokens replenished per window.
    pub sustained_rate: i64,
    /// The window of the sustained rate, as the block spells it.
    pub sustained_window: String,
    /// The maximum the bucket holds: `burst.capacity` or the ADR default, the
    /// sustained rate.
    pub burst_capacity: i64,
    /// The scope of the counter key.
    pub scope: String,
    /// The tokens one request consumes.
    pub cost: i64,
    /// The configured strategy. Every accepted value resolves to the `reject`
    /// behaviour this release implements: nothing branches on this field, so
    /// `queue` and `degrade` produce exactly the refusal `reject` produces.
    pub strategy: String,
    /// Whether the three rate-limit headers are set. The ADR default `true` is
    /// the only value a payload of this release can reach, the validated field
    /// set carrying no `response_headers` field; the `false` branch is the
    /// ADR-level behaviour and is kept implemented.
    pub response_headers: bool,
}

impl EffectiveRateLimit {
    /// Builds the enforcement parameters from the merged rate-limit block
    /// (`cpt-cf-oagw-algo-effective-merge`, `inst-me-10`).
    ///
    /// The ADR defaults are applied to the fields the payload omits:
    /// `algorithm` `token_bucket`, `sustained.window` `second`,
    /// `burst.capacity` the sustained rate, `scope` `tenant`, `strategy`
    /// `reject`, `cost` `1` and `response_headers` `true`. `sharing` takes no
    /// default here, being a control-plane field the merge consumes and this
    /// module never reads, and the `budget` block is not read at all.
    ///
    /// # Errors
    /// Returns the existing `ValidationError` row of `cpt-cf-oagw-algo-error-mapping`
    /// when the block carries no sustained rate, or a window outside the closed
    /// enum — the enforcement-contract violation of a block that names no limit
    /// value to enforce. No row is added to the closed table. The merge cannot
    /// produce such a block, since it collects only limits with a comparable
    /// sustained rate, so the error guards a hand-built block rather than a
    /// reachable merge outcome.
    pub fn from_merged(block: &RateLimitConfig) -> Result<Self, OagwError> {
        let Some(sustained) = block.sustained.as_ref() else {
            return Err(OagwError::validation_error(
                "oagw.rate_limit: the effective rate-limit block carries no sustained rate, so there is no limit value to enforce",
            ));
        };
        let Some(sustained_rate) = sustained.rate else {
            return Err(OagwError::validation_error(
                "oagw.rate_limit: the effective rate-limit block carries a sustained block with no rate, so there is no limit value to enforce",
            ));
        };
        let window = omitted(&sustained.window, WINDOW_SECOND);
        if window_seconds(window).is_none() {
            return Err(OagwError::validation_error(format!(
                "oagw.rate_limit: the sustained window '{window}' is outside the closed enum, so the refill rate cannot be derived"
            )));
        }
        // @cpt-begin:cpt-cf-oagw-algo-token-bucket:p1:inst-tb-03
        // `capacity` comes from `burst.capacity`, reading the ADR default
        // `sustained.rate` when the block does not configure a burst allowance,
        // so the bucket never holds more than the burst it was configured
        // with. A burst below the sustained rate is legal configuration the
        // validated field set does not cross-check and is taken as declared.
        // @cpt-end:cpt-cf-oagw-algo-token-bucket:p1:inst-tb-03
        let burst_capacity = block
            .burst
            .as_ref()
            .and_then(|burst| burst.capacity)
            .unwrap_or(sustained_rate);
        Ok(Self {
            algorithm: omitted(&block.algorithm, ALGORITHM_TOKEN_BUCKET).to_owned(),
            sustained_rate,
            sustained_window: window.to_owned(),
            burst_capacity,
            scope: omitted(&block.scope, SCOPE_TENANT).to_owned(),
            // The cost of one request, the ADR default `1` when the payload
            // omits the field.
            cost: block.cost.unwrap_or(DEFAULT_RATE_COST),
            // The strategy is carried as configured and branches nothing: the
            // only behaviour this release implements is the `reject` one, so
            // `queue` and `degrade` reach the same refusal without a branch.
            strategy: omitted(&block.strategy, STRATEGY_REJECT).to_owned(),
            response_headers: true,
        })
    }

    /// Tokens per second, derived from the effective limit value
    /// (`cpt-cf-oagw-algo-token-bucket`): `sustained.rate` tokens per
    /// `sustained.window`, so one number drives the refill, the
    /// `X-RateLimit-Limit` value and both delay computations.
    #[must_use]
    pub fn refill_rate(&self) -> f64 {
        // @cpt-begin:cpt-cf-oagw-algo-token-bucket:p1:inst-tb-02
        // `second` → rate, `minute` → rate / 60, `hour` → rate / 3600 and
        // `day` → rate / 86400: the window the merge carried is converted once,
        // here, and the quotient that comes out drives the refill, the
        // `X-RateLimit-Limit` value and both delay computations.
        // @cpt-end:cpt-cf-oagw-algo-token-bucket:p1:inst-tb-02
        #[allow(clippy::cast_precision_loss)] // a rate, not a count
        let per_second = self.sustained_rate as f64 / self.sustained_window_secs() as f64;
        per_second
    }

    /// The sustained window normalized to seconds, `0` for a window outside
    /// the closed enum, which [`EffectiveRateLimit::from_merged`] rejects.
    #[must_use]
    pub fn sustained_window_secs(&self) -> u64 {
        window_seconds(&self.sustained_window).unwrap_or_default()
    }

    /// The bucket maximum as the `f64` the arithmetic works in.
    #[must_use]
    pub fn burst_tokens(&self) -> f64 {
        #[allow(clippy::cast_precision_loss)] // a capacity, not a counter
        let capacity = self.burst_capacity as f64;
        capacity
    }

    /// Whether the effective limit is the value `block` produces, which is how
    /// the enforcement point decides that the matched route's own block is the
    /// block the limit came from. The comparison is a value comparison: no
    /// tier identity is carried through the merge, so the attribution reads
    /// the enforcement parameters the block produces.
    fn comes_from(&self, block: &RateLimitConfig) -> bool {
        Self::from_merged(block).is_ok_and(|candidate| candidate == *self)
    }
}

/// Reads one string field of the block, substituting the ADR default when the
/// payload omitted it: `serde` renders the omission as the field's own default
/// and a hand-built block as the empty string.
fn omitted<'a>(value: &'a str, default: &'a str) -> &'a str {
    if value.is_empty() { default } else { value }
}

/// The closed window enum of `cpt-cf-oagw-algo-shape-validation`, normalized
/// to seconds with the mapping `sustained_per_second` uses.
fn window_seconds(window: &str) -> Option<u64> {
    match window {
        WINDOW_SECOND => Some(1),
        WINDOW_MINUTE => Some(60),
        WINDOW_HOUR => Some(3_600),
        WINDOW_DAY => Some(86_400),
        _ => None,
    }
}

/// The lazy refill of `cpt-cf-oagw-algo-token-bucket`: `tokens = min(capacity,
/// tokens + elapsed * refill_rate)` over the monotonic interval since
/// `last_update`, then `last_update = now`.
///
/// There is no background refill task, no ticker and no wall-clock read: a
/// bucket is topped up only when a check touches it, and an idle bucket is
/// topped up to at most `capacity`, never beyond it.
pub fn refill(bucket: &mut TokenBucket, now: Instant) {
    let elapsed = now
        .saturating_duration_since(bucket.last_update)
        .as_secs_f64();
    let topped_up = bucket.tokens + elapsed * bucket.refill_rate;
    bucket.tokens = topped_up.min(bucket.capacity);
    bucket.last_update = now;
}

/// Acquires `cost` tokens from the bucket (`cpt-cf-oagw-algo-token-bucket`):
/// the lazy refill first, then the `tokens >= cost` test.
///
/// The whole cost is taken or none of it: the balance is never negative and no
/// partial acquisition exists, and a refused request leaves the refilled
/// balance in place for the next check.
pub fn acquire(bucket: &mut TokenBucket, cost: i64, now: Instant) -> bool {
    refill(bucket, now);
    #[allow(clippy::cast_precision_loss)] // a count, compared against a balance
    let cost = cost as f64;
    if bucket.tokens >= cost {
        bucket.tokens -= cost;
        true
    } else {
        false
    }
}

/// Grants the fixed-window approximation its window allowance
/// (`cpt-cf-oagw-algo-token-bucket`, the `sliding_window` branch).
///
/// The window is aligned on the entry's own first boundary: `window_start` is
/// the instant the bucket was created and every boundary of `window_secs`
/// after it is a boundary, `window_start` being advanced by whole
/// `window_secs` multiples, which keeps the arithmetic deterministic without a
/// wall-clock anchor. A boundary replaces the balance with
/// `min(sustained_rate, capacity)` — the grant capped by the bucket's declared
/// maximum — and carries no unused token of the earlier windows across it. The
/// entry's creation is its first boundary and starts from the full bucket of
/// `inst-rl-07`, so the grant applies at every boundary after that one.
///
/// The interval since the last check is pinned to zero, so the lazy refill the
/// same [`acquire`] performs adds no token inside a window and the balance
/// only ever moves at a boundary.
pub fn fixed_window_grant(
    bucket: &mut TokenBucket,
    sustained_rate: i64,
    window_secs: u64,
    now: Instant,
) {
    bucket.last_update = now;
    let elapsed = now.saturating_duration_since(bucket.window_start);
    let Some(periods) = elapsed.as_secs().checked_div(window_secs) else {
        return;
    };
    if periods == 0 {
        return;
    }
    // @cpt-begin:cpt-cf-oagw-algo-token-bucket:p1:inst-tb-05
    // The grant is capped by the bucket's declared maximum: `burst.capacity`
    // below `sustained.rate` is legal configuration the validated field set
    // does not cross-check, and no token is carried across the boundary.
    // @cpt-end:cpt-cf-oagw-algo-token-bucket:p1:inst-tb-05
    #[allow(clippy::cast_precision_loss)] // a count, capped by the capacity
    let grant = sustained_rate as f64;
    bucket.tokens = grant.min(bucket.capacity);
    let boundary = Duration::from_secs(periods.saturating_mul(window_secs));
    bucket.window_start = bucket.window_start.checked_add(boundary).unwrap_or(now);
}

/// The seconds `missing` tokens take to arrive at `rate`, rounded up, `0` when
/// the balance is already there or the bucket cannot refill at all.
fn delay_for(missing: f64, rate: f64) -> u64 {
    if missing <= 0.0 || rate <= 0.0 {
        return 0;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let delay = (missing / rate).ceil() as u64;
    delay
}

/// The `X-RateLimit-Reset` delay (`cpt-cf-oagw-algo-token-bucket`): the
/// seconds until the bucket is full again at `refill_rate`, `0` when the
/// bucket is already at capacity.
#[must_use]
pub fn reset_delay(bucket: &TokenBucket) -> u64 {
    delay_for(bucket.capacity - bucket.tokens, bucket.refill_rate)
}

/// The `Retry-After` delay of a refusal (`cpt-cf-oagw-algo-token-bucket`): the
/// seconds until the balance reaches the ceiling the bucket can actually
/// reach, `min(cost, capacity)`, at the same rate.
///
/// A `cost` above `burst.capacity` is therefore never payable: the bucket is
/// already at that ceiling, the computed delay is `0` and every request for
/// the key is refused, which is the outcome the ADR's own `tokens >= cost`
/// test produces. No cross-check of the two fields is invented here, and the
/// delay is never later than the [`reset_delay`] the same response reports.
#[must_use]
pub fn retry_after_delay(bucket: &TokenBucket, cost: i64) -> u64 {
    #[allow(clippy::cast_precision_loss)] // a count, capped by the capacity
    let ceiling = cost as f64;
    let ceiling = ceiling.min(bucket.capacity);
    let delay = delay_for(ceiling - bucket.tokens, bucket.refill_rate);
    delay.min(reset_delay(bucket))
}

/// The three rate-limit headers, as the response-header block of ADR 0003
/// defines them, with the reset value rendered as a delay in seconds rather
/// than as an absolute timestamp (FEATURE §1.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimitHeaders {
    /// The numeric sustained rate of the effective limit.
    pub limit: i64,
    /// The tokens the bucket holds after the decision, floored to an integer,
    /// so a fractional refill never advertises a token it has not granted.
    pub remaining: i64,
    /// The seconds until the bucket refills to capacity at that rate.
    pub reset: u64,
}

/// The header names and values of one response, in the order the ADR block
/// lists them.
#[must_use]
pub fn header_pairs(h: &RateLimitHeaders) -> [(&'static str, String); 3] {
    [
        (HEADER_LIMIT, h.limit.to_string()),
        (HEADER_REMAINING, h.remaining.to_string()),
        (HEADER_RESET, h.reset.to_string()),
    ]
}

/// The decision one check produces for one bucket
/// (`cpt-cf-oagw-algo-token-bucket`).
///
/// The bucket is mutated in place, so the balance this check leaves behind is
/// what the next check that resolves the same key starts from, and the headers
/// carry the balance after that one subtraction. The headers are `None` when
/// `response_headers` is false, which is the ADR-level branch of
/// `cpt-cf-oagw-dod-dual-rate-config`, and the `Retry-After` delay is `0` when
/// the bucket paid the cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    /// Whether the bucket paid the cost.
    pub acquired: bool,
    /// The three rate-limit headers, `None` when they are switched off.
    pub headers: Option<RateLimitHeaders>,
    /// The `Retry-After` delay of a refusal, `0` on an acquisition.
    pub retry_after_secs: u64,
}

/// Runs one check against one bucket
/// (`cpt-cf-oagw-algo-token-bucket`): the grant or refill of the configured
/// algorithm, the cost test, and the delays the response reports.
///
/// The arithmetic stays in memory: no I/O, no store access, no lock beyond the
/// caller's own entry and no clock other than the monotonic one, so a
/// wall-clock adjustment cannot inject tokens.
pub fn decide(bucket: &mut TokenBucket, limit: &EffectiveRateLimit, now: Instant) -> Decision {
    // @cpt-begin:cpt-cf-oagw-algo-token-bucket:p1:inst-tb-04
    // The algorithm is read from the merged block: `sliding_window` is accepted
    // configuration enforced as the aligned fixed-window approximation, the
    // default `token_bucket` being the exact algorithm of this release.
    // @cpt-end:cpt-cf-oagw-algo-token-bucket:p1:inst-tb-04
    if limit.algorithm == ALGORITHM_SLIDING_WINDOW {
        let window_secs = limit.sustained_window_secs();
        fixed_window_grant(bucket, limit.sustained_rate, window_secs, now);
    }
    // @cpt-begin:cpt-cf-oagw-algo-token-bucket:p1:inst-tb-06
    // The token bucket refills lazily on access; the fixed-window
    // approximation has already pinned the interval since the last check to
    // zero, so this adds nothing inside a window.
    // @cpt-end:cpt-cf-oagw-algo-token-bucket:p1:inst-tb-06
    // @cpt-begin:cpt-cf-oagw-algo-token-bucket:p1:inst-tb-07
    // The refilled balance is tested against the cost.
    // @cpt-end:cpt-cf-oagw-algo-token-bucket:p1:inst-tb-07
    let acquired = acquire(bucket, limit.cost, now);
    // @cpt-begin:cpt-cf-oagw-algo-token-bucket:p1:inst-tb-10
    // The reset delay the headers report is the seconds until the bucket is
    // full again at `refill_rate`, and the `Retry-After` delay of a refusal is
    // the seconds until the balance reaches `min(cost, capacity)`.
    // @cpt-end:cpt-cf-oagw-algo-token-bucket:p1:inst-tb-10
    let headers = limit.response_headers.then(|| RateLimitHeaders {
        limit: limit.sustained_rate,
        remaining: floored_tokens(bucket.tokens),
        reset: reset_delay(bucket),
    });
    // @cpt-begin:cpt-cf-oagw-algo-token-bucket:p1:inst-tb-11
    // The arithmetic is in memory only: no I/O, no store access, no lock
    // beyond the caller's own entry and no clock other than the monotonic one.
    // @cpt-end:cpt-cf-oagw-algo-token-bucket:p1:inst-tb-11
    // @cpt-begin:cpt-cf-oagw-algo-token-bucket:p1:inst-tb-12
    // The decision, the post-decision balance in the headers and the computed
    // delays go back to the caller, which renders the outcome.
    // @cpt-end:cpt-cf-oagw-algo-token-bucket:p1:inst-tb-12
    if acquired {
        // @cpt-begin:cpt-cf-oagw-algo-token-bucket:p1:inst-tb-08
        // Acquired: the whole cost was taken, and the remaining count the
        // header reports is the balance after that one subtraction.
        // @cpt-end:cpt-cf-oagw-algo-token-bucket:p1:inst-tb-08
        Decision {
            acquired: true,
            headers,
            retry_after_secs: 0,
        }
    } else {
        // @cpt-begin:cpt-cf-oagw-algo-token-bucket:p1:inst-tb-09
        // Exhausted: the balance is unchanged and stays in place for the next
        // check, never negative and never partially spent.
        // @cpt-end:cpt-cf-oagw-algo-token-bucket:p1:inst-tb-09
        let retry_after_secs = retry_after_delay(bucket, limit.cost);
        Decision {
            acquired: false,
            headers,
            retry_after_secs,
        }
    }
}

/// The integer token count the headers report: the floored balance.
fn floored_tokens(tokens: f64) -> i64 {
    #[allow(clippy::cast_possible_truncation)] // the header is an integer count
    let floored = tokens.floor() as i64;
    floored
}

/// The caller-observable result of one check
/// (`cpt-cf-oagw-flow-rate-limit-check`).
///
/// The caller — the proxy pipeline — renders these: an admission continues
/// into the guard and transform phases, a refusal is rendered through the
/// existing `RateLimitExceeded` row of `cpt-cf-oagw-algo-error-mapping` and
/// reaches neither a plugin nor an upstream. This type carries the decision
/// and adds no HTTP surface of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RateLimitOutcome {
    /// No effective limit: no bucket, no counter and no rate-limit header.
    NotLimited,
    /// The bucket paid the cost; the request continues.
    Admitted {
        /// The three headers, `None` when `response_headers` is false.
        headers: Option<RateLimitHeaders>,
    },
    /// The bucket cannot pay the cost; the caller renders the 429.
    Refused {
        /// The `Retry-After` delay in seconds.
        retry_after_secs: u64,
        /// The three headers, `None` when `response_headers` is false.
        headers: Option<RateLimitHeaders>,
    },
}

/// The enforcement point one request resolved
/// (`cpt-cf-oagw-algo-scope-key`): the selected upstream and the matched
/// route, which together decide the `{resource_type}:{resource_id}` prefix of
/// the counter key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnforcementPoint {
    /// The identifier of the upstream the request was routed to.
    pub upstream_id: Uuid,
    /// The route the pipeline matched, `None` when the upstream was matched
    /// without one.
    pub matched_route: Option<Route>,
}

/// The subject dimensions of one request, extracted from its security context
/// and the connection it arrived on.
///
/// No request content beyond these three fields and the enforcement point
/// enters the counter key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScopeContext {
    /// The subject tenant the request was authenticated into.
    pub subject_tenant_id: Uuid,
    /// The authenticated subject of the request.
    pub subject_id: Uuid,
    /// The peer address of the connection, the only address the `ip` scope
    /// ever counts by.
    pub peer: IpAddr,
}

/// Resolves the counter scope key of the registry entry
/// (`cpt-cf-oagw-algo-scope-key`).
///
/// The key is `oagw:ratelimit:{resource_type}:{resource_id}:{scope}:{scope_id}:{window}`,
/// the component list of `cpt-cf-oagw-adr-rate-limiting` minus the trailing
/// date-bucket component the deferred sync protocol appends. The `{window}`
/// component is kept so two limits that differ only in window never share a
/// counter, and the UUIDs render in their canonical hyphenated form.
#[must_use]
pub fn resolve_scope_key(
    limit: &EffectiveRateLimit,
    point: &EnforcementPoint,
    ctx: &ScopeContext,
) -> String {
    // @cpt-begin:cpt-cf-oagw-algo-scope-key:p1:inst-sk-01
    // `scope` comes from the merged block, reading the ADR default `tenant`
    // when the validated block omitted the field. The enum is already enforced
    // at persist time by `cpt-cf-oagw-algo-shape-validation`, so no value
    // outside it reaches this step and none is re-validated here.
    // @cpt-end:cpt-cf-oagw-algo-scope-key:p1:inst-sk-01
    // @cpt-begin:cpt-cf-oagw-algo-scope-key:p1:inst-sk-02
    // The key is composed from the components the ADR states: the enforcement
    // point as `{resource_type}:{resource_id}`, the configured scope, the
    // subject dimension the scope selects and the sustained window of the
    // effective limit.
    // @cpt-end:cpt-cf-oagw-algo-scope-key:p1:inst-sk-02
    let (resource_type, resource_id) = enforcement_point(limit, point);
    let scope_id = scope_id(limit, point, ctx);
    // @cpt-begin:cpt-cf-oagw-algo-scope-key:p1:inst-sk-13
    // Nothing else is derived from the request: no header, no query parameter,
    // no path segment and no caller-supplied identifier enters the key beyond
    // the fields above, so a caller cannot steer the counter it is counted
    // into.
    // @cpt-end:cpt-cf-oagw-algo-scope-key:p1:inst-sk-13
    // @cpt-begin:cpt-cf-oagw-algo-scope-key:p1:inst-sk-14
    // The same scope inputs always produce the same key, and two keys that
    // differ in any component never name the same bucket.
    // @cpt-end:cpt-cf-oagw-algo-scope-key:p1:inst-sk-14
    // @cpt-begin:cpt-cf-oagw-algo-scope-key:p1:inst-sk-15
    // The key goes back to the caller, which looks it up in the registry.
    // @cpt-end:cpt-cf-oagw-algo-scope-key:p1:inst-sk-15
    let scope = limit.scope.as_str();
    let window = limit.sustained_window.as_str();
    format!("{KEY_PREFIX}{resource_type}:{resource_id}:{scope}:{scope_id}:{window}")
}

/// The `{resource_type}:{resource_id}` of the enforcement point: the matched
/// route's id under `route` when the effective block is the route's, the
/// selected upstream's id under `upstream` otherwise.
fn enforcement_point(
    limit: &EffectiveRateLimit,
    point: &EnforcementPoint,
) -> (&'static str, String) {
    let route_id = point
        .matched_route
        .as_ref()
        .filter(|route| route_is_effective(limit, route))
        .and_then(|route| route.id);
    match route_id {
        Some(route_id) => (RESOURCE_TYPE_ROUTE, route_id.to_string()),
        None => (RESOURCE_TYPE_UPSTREAM, point.upstream_id.to_string()),
    }
}

/// Whether the matched route's own block is the block the effective limit came
/// from, decided by value comparison: the merge carries the winning block as a
/// value and no tier identity, so the attribution reads the enforcement
/// parameters the block produces.
fn route_is_effective(limit: &EffectiveRateLimit, route: &Route) -> bool {
    route
        .rate_limit
        .as_ref()
        .is_some_and(|block| limit.comes_from(block))
}

/// The `{scope_id}` the configured scope selects
/// (`cpt-cf-oagw-algo-scope-key`).
fn scope_id(limit: &EffectiveRateLimit, point: &EnforcementPoint, ctx: &ScopeContext) -> String {
    match limit.scope.as_str() {
        SCOPE_GLOBAL => global_scope_id(),
        SCOPE_TENANT => tenant_scope_id(ctx),
        SCOPE_USER => user_scope_id(ctx),
        SCOPE_IP => ip_scope_id(ctx),
        // The enum is closed at persist time, so the fall-through of the chain
        // is the `route` scope, the last value of
        // `crate::domain::model::SCOPE_ROUTE`.
        _ => route_scope_id(point),
    }
}

/// The constant `scope_id` of the `global` scope: one counter per enforcement
/// point and window, shared by every subject and every tenant of the instance.
fn global_scope_id() -> String {
    // @cpt-begin:cpt-cf-oagw-algo-scope-key:p1:inst-sk-03
    // The `global` branch of the scope chain.
    // @cpt-end:cpt-cf-oagw-algo-scope-key:p1:inst-sk-03
    // @cpt-begin:cpt-cf-oagw-algo-scope-key:p1:inst-sk-04
    // The constant `global`: one counter per enforcement point and window,
    // shared by every subject and every tenant of this instance and by no other
    // instance.
    // @cpt-end:cpt-cf-oagw-algo-scope-key:p1:inst-sk-04
    SCOPE_GLOBAL.to_owned()
}

/// The subject tenant id, so two tenants never share a bucket.
fn tenant_scope_id(ctx: &ScopeContext) -> String {
    // @cpt-begin:cpt-cf-oagw-algo-scope-key:p1:inst-sk-05
    // The `tenant` branch of the scope chain, the ADR default.
    // @cpt-end:cpt-cf-oagw-algo-scope-key:p1:inst-sk-05
    // @cpt-begin:cpt-cf-oagw-algo-scope-key:p1:inst-sk-06
    // The subject tenant id of the request's security context: two tenants
    // never share a bucket and one tenant's traffic cannot exhaust another
    // tenant's.
    // @cpt-end:cpt-cf-oagw-algo-scope-key:p1:inst-sk-06
    let tenant = ctx.subject_tenant_id;
    tenant.to_string()
}

/// The subject id qualified by its subject tenant id, so a subject identifier
/// that recurs in another tenant cannot reach that tenant's bucket.
fn user_scope_id(ctx: &ScopeContext) -> String {
    // @cpt-begin:cpt-cf-oagw-algo-scope-key:p1:inst-sk-07
    // The `user` branch of the scope chain.
    // @cpt-end:cpt-cf-oagw-algo-scope-key:p1:inst-sk-07
    // @cpt-begin:cpt-cf-oagw-algo-scope-key:p1:inst-sk-08
    // The security-context subject id qualified by its subject tenant id, so a
    // subject identifier that recurs in another tenant cannot reach that
    // tenant's bucket.
    // @cpt-end:cpt-cf-oagw-algo-scope-key:p1:inst-sk-08
    let tenant = ctx.subject_tenant_id;
    let subject = ctx.subject_id;
    format!("{tenant}/{subject}")
}

/// The peer address of the connection in its canonical textual form: no
/// forwarded or caller-supplied header contributes to it.
fn ip_scope_id(ctx: &ScopeContext) -> String {
    // @cpt-begin:cpt-cf-oagw-algo-scope-key:p1:inst-sk-09
    // The `ip` branch of the scope chain.
    // @cpt-end:cpt-cf-oagw-algo-scope-key:p1:inst-sk-09
    // @cpt-begin:cpt-cf-oagw-algo-scope-key:p1:inst-sk-10
    // The peer address of the connection the request arrived on, in its
    // canonical textual form, with no forwarded or caller-supplied header
    // contributing to it per §1.5.
    // @cpt-end:cpt-cf-oagw-algo-scope-key:p1:inst-sk-10
    let peer = ctx.peer;
    peer.to_string()
}

/// The (upstream, route) pair the pipeline matched, so a counter is per route
/// and never shared with the upstream's other routes. When no route was
/// matched the pair degenerates to the upstream id.
fn route_scope_id(point: &EnforcementPoint) -> String {
    // @cpt-begin:cpt-cf-oagw-algo-scope-key:p1:inst-sk-11
    // The `route` branch of the scope chain, the fall-through of the closed
    // enum.
    // @cpt-end:cpt-cf-oagw-algo-scope-key:p1:inst-sk-11
    // @cpt-begin:cpt-cf-oagw-algo-scope-key:p1:inst-sk-12
    // The (upstream, route) pair the proxy pipeline matched, so a counter is
    // per route and never shared with the upstream's other routes or with the
    // upstream's own unscoped counter.
    // @cpt-end:cpt-cf-oagw-algo-scope-key:p1:inst-sk-12
    let route_id = point.matched_route.as_ref().and_then(|route| route.id);
    match route_id {
        Some(route_id) => {
            let upstream = point.upstream_id;
            format!("{upstream}/{route_id}")
        }
        None => {
            let upstream = point.upstream_id;
            upstream.to_string()
        }
    }
}

// @cpt-end:cpt-cf-oagw-dod-dual-rate-config:p1:inst-full

// @cpt-anchored-tests

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    use std::time::Duration;

    use super::*;
    use crate::domain::model::{
        BurstCapacity, SCOPE_ROUTE, SHARING_ENFORCE, SHARING_PRIVATE, STRATEGY_DEGRADE,
        STRATEGY_QUEUE, SustainedRate,
    };

    const UPSTREAM: Uuid = Uuid::from_u128(0x0a11_ce5a_0000_0001);
    const ROUTE_A: Uuid = Uuid::from_u128(0x0a11_ce5a_0000_0002);
    const ROUTE_B: Uuid = Uuid::from_u128(0x0a11_ce5a_0000_0003);
    const TENANT: Uuid = Uuid::from_u128(0x0a11_ce5a_0000_0004);
    const OTHER_TENANT: Uuid = Uuid::from_u128(0x0a11_ce5a_0000_0005);
    const SUBJECT: Uuid = Uuid::from_u128(0x0a11_ce5a_0000_0006);
    const OTHER_SUBJECT: Uuid = Uuid::from_u128(0x0a11_ce5a_0000_0007);
    const PEER: IpAddr = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 7));
    const OTHER_PEER: IpAddr = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 8));
    const IPV6_PEER: IpAddr = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1));

    /// The instant every test anchors its durations on: the arithmetic is
    /// relative, so the monotonic clock of the host is only a base.
    fn start() -> Instant {
        Instant::now()
    }

    /// The balance comparison of these tests: the expectations are
    /// integer-valued, and no `f64` equality is written, so the tests compare
    /// within a tolerance.
    fn near(actual: f64, expected: f64) -> bool {
        (actual - expected).abs() < 1e-9
    }

    /// A block whose every optional field is omitted, the shape `serde`
    /// produces for a payload that only carries the sustained rate.
    fn bare(rate: i64) -> RateLimitConfig {
        RateLimitConfig {
            sustained: Some(SustainedRate {
                rate: Some(rate),
                window: String::new(),
            }),
            ..RateLimitConfig::default()
        }
    }

    /// A block with the sustained rate and window set and every other field
    /// left to the serde default.
    fn block(rate: i64, window: &str) -> RateLimitConfig {
        RateLimitConfig {
            sustained: Some(SustainedRate {
                rate: Some(rate),
                window: window.to_owned(),
            }),
            ..RateLimitConfig::default()
        }
    }

    fn with_burst(mut block: RateLimitConfig, capacity: i64) -> RateLimitConfig {
        block.burst = Some(BurstCapacity {
            capacity: Some(capacity),
        });
        block
    }

    fn with_cost(mut block: RateLimitConfig, cost: i64) -> RateLimitConfig {
        block.cost = Some(cost);
        block
    }

    fn with_scope(mut block: RateLimitConfig, scope: &str) -> RateLimitConfig {
        block.scope = scope.to_owned();
        block
    }

    fn with_strategy(mut block: RateLimitConfig, strategy: &str) -> RateLimitConfig {
        block.strategy = strategy.to_owned();
        block
    }

    fn with_algorithm(mut block: RateLimitConfig, algorithm: &str) -> RateLimitConfig {
        block.algorithm = algorithm.to_owned();
        block
    }

    /// A block with the sharing mode set, the control-plane field the merge
    /// consumes and this module never reads.
    fn with_sharing(mut block: RateLimitConfig, sharing: &str) -> RateLimitConfig {
        block.sharing = sharing.to_owned();
        block
    }

    fn limit(block: &RateLimitConfig) -> EffectiveRateLimit {
        EffectiveRateLimit::from_merged(block).expect("the fixture carries a sustained rate")
    }

    fn point(upstream: Uuid) -> EnforcementPoint {
        EnforcementPoint {
            upstream_id: upstream,
            matched_route: None,
        }
    }

    fn routed(upstream: Uuid, route: Route) -> EnforcementPoint {
        EnforcementPoint {
            upstream_id: upstream,
            matched_route: Some(route),
        }
    }

    fn route(id: Uuid, upstream: Uuid, block: Option<RateLimitConfig>) -> Route {
        Route {
            id: Some(id),
            upstream_id: Some(upstream),
            rate_limit: block,
            ..Route::default()
        }
    }

    fn ctx() -> ScopeContext {
        ScopeContext {
            subject_tenant_id: TENANT,
            subject_id: SUBJECT,
            peer: PEER,
        }
    }

    fn full_bucket(l: &EffectiveRateLimit, now: Instant) -> TokenBucket {
        TokenBucket {
            tokens: l.burst_tokens(),
            last_update: now,
            capacity: l.burst_tokens(),
            refill_rate: l.refill_rate(),
            window_start: now,
        }
    }

    #[test]
    fn the_effective_limit_reads_the_numeric_core_of_the_merged_block() {
        let per_minute = limit(&block(1_000, WINDOW_MINUTE));
        assert_eq!(per_minute.sustained_rate, 1_000);
        assert_eq!(per_minute.sustained_window, WINDOW_MINUTE);
        assert_eq!(per_minute.sustained_window_secs(), 60);
        assert!(near(per_minute.refill_rate(), 1_000.0 / 60.0));
    }

    #[test]
    fn the_refill_rate_is_the_sustained_rate_per_second() {
        let per_second = limit(&block(100, WINDOW_SECOND));
        assert!(near(per_second.refill_rate(), 100.0));
        let per_hour = limit(&block(3_600, WINDOW_HOUR));
        assert!(near(per_hour.refill_rate(), 1.0));
        let per_day = limit(&block(86_400, WINDOW_DAY));
        assert!(near(per_day.refill_rate(), 1.0));
    }

    #[test]
    fn the_burst_capacity_defaults_to_the_sustained_rate() {
        let without_burst = limit(&block(100, WINDOW_SECOND));
        assert_eq!(without_burst.burst_capacity, 100);
        assert!(near(without_burst.burst_tokens(), 100.0));
        let with_burst = limit(&with_burst(block(100, WINDOW_SECOND), 500));
        assert_eq!(with_burst.burst_capacity, 500);
    }

    #[test]
    fn a_burst_below_the_sustained_rate_is_taken_as_declared() {
        let l = limit(&with_burst(block(100, WINDOW_SECOND), 10));
        assert_eq!(l.burst_capacity, 10);
        assert_eq!(l.sustained_rate, 100);
    }

    #[test]
    fn the_cost_defaults_to_one_token() {
        assert_eq!(DEFAULT_RATE_COST, 1);
        let without_cost = limit(&block(100, WINDOW_SECOND));
        assert_eq!(without_cost.cost, DEFAULT_RATE_COST);
        let with_cost = limit(&with_cost(block(100, WINDOW_SECOND), 10));
        assert_eq!(with_cost.cost, 10);
    }

    #[test]
    fn every_omitted_field_takes_its_adr_default() {
        let l = limit(&bare(100));
        assert_eq!(l.algorithm, ALGORITHM_TOKEN_BUCKET);
        assert_eq!(l.sustained_window, WINDOW_SECOND);
        assert_eq!(l.sustained_window_secs(), 1);
        assert_eq!(l.burst_capacity, 100);
        assert_eq!(l.scope, SCOPE_TENANT);
        assert_eq!(l.strategy, STRATEGY_REJECT);
        assert_eq!(l.cost, 1);
        assert!(
            l.response_headers,
            "the ADR default is the only reachable value"
        );
    }

    #[test]
    fn the_strategy_is_carried_and_branches_nothing() {
        let reject = limit(&with_strategy(block(100, WINDOW_SECOND), STRATEGY_REJECT));
        let queue = limit(&with_strategy(block(100, WINDOW_SECOND), STRATEGY_QUEUE));
        let degrade = limit(&with_strategy(block(100, WINDOW_SECOND), STRATEGY_DEGRADE));
        for strategy in [reject, queue, degrade] {
            assert_eq!(strategy.algorithm, ALGORITHM_TOKEN_BUCKET);
            assert_eq!(strategy.sustained_rate, 100);
            assert_eq!(strategy.burst_capacity, 100);
            assert_eq!(strategy.cost, 1);
            assert_eq!(strategy.scope, SCOPE_TENANT);
            assert!(strategy.response_headers);
        }
    }

    #[test]
    fn a_block_without_a_sustained_rate_is_an_enforcement_contract_violation() {
        let absent = RateLimitConfig {
            sustained: None,
            ..RateLimitConfig::default()
        };
        let error = EffectiveRateLimit::from_merged(&absent)
            .expect_err("a block with no sustained rate names no limit to enforce");
        assert_eq!(error.mapping().variant, "ValidationError");
        assert_eq!(error.status(), 400);
        assert!(!error.is_retriable());

        let mut unshaped = block(100, WINDOW_SECOND);
        unshaped.sustained = Some(SustainedRate {
            rate: None,
            window: WINDOW_SECOND.to_owned(),
        });
        assert!(EffectiveRateLimit::from_merged(&unshaped).is_err());
    }

    #[test]
    fn a_window_outside_the_closed_enum_cannot_drive_a_refill_rate() {
        let week = block(100, "week");
        let error = EffectiveRateLimit::from_merged(&week)
            .expect_err("no seconds can be derived from a window outside the enum");
        assert_eq!(error.mapping().variant, "ValidationError");
    }

    // @cpt-anchored-tests-2

    #[test]
    fn an_idle_bucket_refills_to_at_most_capacity_and_never_beyond() {
        let now = start();
        let l = limit(&block(10, WINDOW_SECOND));
        let mut b = full_bucket(&l, now);
        b.tokens = 4.0;
        refill(&mut b, now + Duration::from_secs(3_600));
        assert!(near(b.tokens, 10.0));
        assert_eq!(b.last_update, now + Duration::from_secs(3_600));
    }

    #[test]
    fn the_refill_adds_exactly_the_elapsed_interval_times_the_rate() {
        let now = start();
        let l = limit(&block(2, WINDOW_SECOND));
        let mut b = full_bucket(&l, now);
        b.capacity = 100.0;
        b.tokens = 4.0;
        refill(&mut b, now + Duration::from_secs(3));
        assert!(near(b.tokens, 10.0));
    }

    #[test]
    fn an_access_at_the_last_update_instant_moves_no_balance() {
        let now = start();
        let l = limit(&block(2, WINDOW_SECOND));
        let mut b = full_bucket(&l, now);
        b.capacity = 100.0;
        b.tokens = 4.0;
        refill(&mut b, now);
        assert!(near(b.tokens, 4.0));
        assert_eq!(b.last_update, now);
    }

    #[test]
    fn an_acquisition_takes_the_whole_cost_or_none_of_it() {
        let now = start();
        let l = limit(&block(10, WINDOW_SECOND));
        let mut b = full_bucket(&l, now);
        b.tokens = 5.0;
        assert!(acquire(&mut b, 3, now));
        assert!(near(b.tokens, 2.0));
        assert!(!acquire(&mut b, 3, now));
        assert!(
            near(b.tokens, 2.0),
            "no partial acquisition and no negative balance"
        );
    }

    #[test]
    fn a_bucket_at_zero_recovers_only_through_the_refill() {
        let now = start();
        let l = limit(&block(10, WINDOW_SECOND));
        let mut b = full_bucket(&l, now);
        for _ in 0..10 {
            assert!(acquire(&mut b, 1, now));
        }
        assert!(near(b.tokens, 0.0));
        assert!(!acquire(&mut b, 1, now), "no reset to full on demand");
        assert!(near(b.tokens, 0.0));
        assert!(acquire(&mut b, 1, now + Duration::from_millis(500)));
        assert!(near(b.tokens, 4.0), "half a second at 10 tokens per second");
    }

    #[test]
    fn the_first_request_is_charged_against_a_full_bucket() {
        let now = start();
        let l = limit(&block(10, WINDOW_SECOND));
        let mut b = full_bucket(&l, now);
        assert!(near(b.tokens, 10.0));
        assert!(acquire(&mut b, 1, now));
        assert!(near(b.tokens, 9.0));
    }

    #[test]
    fn the_reset_delay_is_zero_while_the_bucket_is_at_capacity() {
        let now = start();
        let l = limit(&block(10, WINDOW_SECOND));
        let b = full_bucket(&l, now);
        assert_eq!(reset_delay(&b), 0);
    }

    #[test]
    fn the_reset_delay_is_the_ceil_of_the_missing_tokens_over_the_rate() {
        let now = start();
        let l = limit(&block(1, WINDOW_SECOND));
        let mut b = full_bucket(&l, now);
        b.capacity = 10.0;
        b.tokens = 7.5;
        assert_eq!(reset_delay(&b), 3);
    }

    #[test]
    fn the_retry_after_is_never_later_than_the_reset_delay() {
        let now = start();
        let l = limit(&block(1, WINDOW_SECOND));
        let mut b = full_bucket(&l, now);
        b.capacity = 10.0;
        b.tokens = 2.0;
        let retry = retry_after_delay(&b, 3);
        assert_eq!(retry, 1);
        assert!(retry <= reset_delay(&b));
    }

    #[test]
    fn a_cost_above_the_capacity_is_refused_permanently_with_a_zero_delay() {
        let now = start();
        let l = limit(&with_cost(block(1, WINDOW_SECOND), 50));
        let mut b = full_bucket(&l, now);
        assert!(near(b.capacity, 1.0));
        assert!(
            !acquire(&mut b, 50, now),
            "a full bucket cannot pay the cost"
        );
        let later = decide(&mut b, &l, now + Duration::from_secs(3_600));
        assert!(!later.acquired, "no delay makes the cost payable");
        assert_eq!(later.retry_after_secs, 0);
    }

    #[test]
    fn the_headers_carry_the_limit_the_remaining_and_the_reset() {
        let now = start();
        let l = limit(&block(100, WINDOW_SECOND));
        let mut b = full_bucket(&l, now);
        let decision = decide(&mut b, &l, now);
        assert!(decision.acquired);
        assert_eq!(decision.retry_after_secs, 0);
        let headers = decision.headers.expect("the ADR default sets them");
        assert_eq!(headers.limit, 100);
        assert_eq!(headers.remaining, 99);
        assert_eq!(headers.reset, 1);
        let pairs = header_pairs(&headers);
        assert_eq!(pairs[0].0, HEADER_LIMIT);
        assert_eq!(pairs[0].1, "100");
        assert_eq!(pairs[1].0, HEADER_REMAINING);
        assert_eq!(pairs[1].1, "99");
        assert_eq!(pairs[2].0, HEADER_RESET);
        assert_eq!(pairs[2].1, "1");
        assert_eq!(HEADER_RETRY_AFTER, "Retry-After");
    }

    #[test]
    fn a_fractional_balance_never_advertises_a_token_it_has_not_granted() {
        let now = start();
        let l = limit(&with_cost(block(10, WINDOW_SECOND), 2));
        let mut b = full_bucket(&l, now);
        b.tokens = 0.0;
        let half = now + Duration::from_millis(50);
        assert!(!acquire(&mut b, 2, half), "0.5 tokens cannot pay 2");
        assert!(near(b.tokens, 0.5));
        let decision = decide(&mut b, &l, half);
        assert!(!decision.acquired);
        let headers = decision.headers.expect("the ADR default sets them");
        assert_eq!(headers.remaining, 0, "the floor of 0.5");
        let more = half + Duration::from_millis(100);
        let decision = decide(&mut b, &l, more);
        assert!(!decision.acquired, "1.5 tokens cannot pay 2");
        let headers = decision.headers.expect("the ADR default sets them");
        assert_eq!(headers.remaining, 1, "the floor of 1.5");
    }

    #[test]
    fn the_adr_level_false_branch_sets_no_header_and_still_refuses() {
        let now = start();
        let l = limit(&block(100, WINDOW_SECOND));
        let mut without_headers = l.clone();
        without_headers.response_headers = false;
        let mut b = full_bucket(&l, now);
        let admitted = decide(&mut b, &without_headers, now);
        assert!(admitted.acquired);
        assert!(admitted.headers.is_none());
        b.tokens = 0.0;
        let refused = decide(&mut b, &without_headers, now);
        assert!(!refused.acquired);
        assert!(refused.headers.is_none());
        assert!(refused.retry_after_secs > 0);
    }

    // @cpt-anchored-tests-3

    #[test]
    fn a_sliding_window_block_grants_its_window_allowance_at_each_boundary() {
        let now = start();
        let l = limit(&with_algorithm(
            block(10, WINDOW_MINUTE),
            ALGORITHM_SLIDING_WINDOW,
        ));
        let mut b = full_bucket(&l, now);
        for index in 0..10 {
            let decision = decide(&mut b, &l, now);
            assert!(decision.acquired, "request {index} of the first window");
        }
        assert!(near(b.tokens, 0.0));
        let mid_window = now + Duration::from_secs(30);
        let refused = decide(&mut b, &l, mid_window);
        assert!(!refused.acquired, "no token is refilled inside a window");
        assert!(near(b.tokens, 0.0));
        let boundary = now + Duration::from_secs(60);
        let granted = decide(&mut b, &l, boundary);
        assert!(granted.acquired, "the boundary grants the window allowance");
        assert!(
            near(b.tokens, 9.0),
            "nothing of the earlier window is carried"
        );
    }

    #[test]
    fn the_window_is_aligned_on_the_entry_s_own_first_boundary() {
        let now = start();
        let l = limit(&with_algorithm(
            block(5, WINDOW_MINUTE),
            ALGORITHM_SLIDING_WINDOW,
        ));
        let mut b = full_bucket(&l, now);
        for _ in 0..5 {
            assert!(decide(&mut b, &l, now).acquired);
        }
        let before = now + Duration::from_secs(59);
        assert!(!decide(&mut b, &l, before).acquired);
        assert!(near(b.tokens, 0.0));
        let after = now + Duration::from_secs(61);
        assert!(
            decide(&mut b, &l, after).acquired,
            "one window past the start"
        );
        assert!(near(b.tokens, 4.0));
    }

    #[test]
    fn the_grant_is_capped_by_the_declared_burst_capacity() {
        let now = start();
        let configured = with_burst(
            with_algorithm(block(100, WINDOW_MINUTE), ALGORITHM_SLIDING_WINDOW),
            10,
        );
        let l = limit(&configured);
        let mut b = full_bucket(&l, now);
        for _ in 0..10 {
            assert!(decide(&mut b, &l, now).acquired);
        }
        assert!(near(b.tokens, 0.0));
        let boundary = now + Duration::from_secs(60);
        let granted = decide(&mut b, &l, boundary);
        assert!(granted.acquired);
        assert!(
            near(b.tokens, 9.0),
            "the grant is the capacity, not the 100 tokens of the sustained rate"
        );
    }

    #[test]
    fn the_first_window_starts_from_the_full_bucket_of_inst_rl_07() {
        let now = start();
        let configured = with_burst(
            with_algorithm(block(10, WINDOW_MINUTE), ALGORITHM_SLIDING_WINDOW),
            100,
        );
        let l = limit(&configured);
        let mut b = full_bucket(&l, now);
        let decision = decide(&mut b, &l, now);
        assert!(decision.acquired);
        let headers = decision.headers.expect("the ADR default sets them");
        assert_eq!(
            headers.remaining, 99,
            "the creation starts at the burst capacity"
        );
    }

    #[test]
    fn the_approximation_reports_the_same_refusal_shape_as_the_exact_algorithm() {
        let now = start();
        let exact = limit(&block(10, WINDOW_MINUTE));
        let approximated = limit(&with_algorithm(
            block(10, WINDOW_MINUTE),
            ALGORITHM_SLIDING_WINDOW,
        ));
        let mut exact_bucket = full_bucket(&exact, now);
        let mut approximated_bucket = full_bucket(&approximated, now);
        for _ in 0..10 {
            assert!(decide(&mut exact_bucket, &exact, now).acquired);
            assert!(decide(&mut approximated_bucket, &approximated, now).acquired);
        }
        let refused_exact = decide(&mut exact_bucket, &exact, now);
        let refused_approximated = decide(&mut approximated_bucket, &approximated, now);
        assert!(!refused_exact.acquired);
        assert!(!refused_approximated.acquired);
        assert_eq!(
            refused_exact.retry_after_secs,
            refused_approximated.retry_after_secs
        );
        assert_eq!(refused_exact.headers, refused_approximated.headers);
    }

    // @cpt-anchored-tests-4

    #[test]
    fn the_key_is_the_adr_component_list_without_the_date_bucket() {
        let l = limit(&block(10, WINDOW_MINUTE));
        let key = resolve_scope_key(&l, &point(UPSTREAM), &ctx());
        assert!(key.starts_with(KEY_PREFIX));
        let components: Vec<&str> = key.split(':').collect();
        assert_eq!(components.len(), 7);
        assert_eq!(components[0], "oagw");
        assert_eq!(components[1], "ratelimit");
        assert_eq!(components[2], RESOURCE_TYPE_UPSTREAM);
        assert_eq!(components[3], UPSTREAM.to_string());
        assert_eq!(components[4], SCOPE_TENANT);
        assert_eq!(components[5], TENANT.to_string());
        assert_eq!(components[6], WINDOW_MINUTE);
    }

    #[test]
    fn the_global_scope_shares_one_counter_per_enforcement_point_and_window() {
        let l = limit(&with_scope(block(10, WINDOW_MINUTE), SCOPE_GLOBAL));
        let key = resolve_scope_key(&l, &point(UPSTREAM), &ctx());
        let other = ScopeContext {
            subject_tenant_id: OTHER_TENANT,
            subject_id: OTHER_SUBJECT,
            peer: OTHER_PEER,
        };
        assert_eq!(key, resolve_scope_key(&l, &point(UPSTREAM), &other));
        assert_eq!(key.split(':').nth(5), Some(SCOPE_GLOBAL));
    }

    #[test]
    fn two_tenants_never_share_a_tenant_counter() {
        let l = limit(&with_scope(block(10, WINDOW_MINUTE), SCOPE_TENANT));
        let key = resolve_scope_key(&l, &point(UPSTREAM), &ctx());
        let other = ScopeContext {
            subject_tenant_id: OTHER_TENANT,
            ..ctx()
        };
        assert_ne!(key, resolve_scope_key(&l, &point(UPSTREAM), &other));
        assert_eq!(
            key,
            resolve_scope_key(&l, &point(UPSTREAM), &ctx()),
            "the same scope inputs produce the same key"
        );
    }

    #[test]
    fn a_subject_is_counted_separately_and_qualified_by_its_tenant() {
        let l = limit(&with_scope(block(10, WINDOW_MINUTE), SCOPE_USER));
        let key = resolve_scope_key(&l, &point(UPSTREAM), &ctx());
        let expected = format!("{KEY_PREFIX}upstream:{UPSTREAM}:user:{TENANT}/{SUBJECT}:minute");
        assert_eq!(key, expected);
        let recurring = ScopeContext {
            subject_tenant_id: OTHER_TENANT,
            subject_id: SUBJECT,
            peer: PEER,
        };
        assert_ne!(
            key,
            resolve_scope_key(&l, &point(UPSTREAM), &recurring),
            "a subject identifier that recurs in another tenant resolves another key"
        );
        let other_subject = ScopeContext {
            subject_id: OTHER_SUBJECT,
            ..ctx()
        };
        assert_ne!(key, resolve_scope_key(&l, &point(UPSTREAM), &other_subject));
    }

    #[test]
    fn the_ip_scope_counts_by_the_peer_address_alone() {
        let l = limit(&with_scope(block(10, WINDOW_MINUTE), SCOPE_IP));
        let key = resolve_scope_key(&l, &point(UPSTREAM), &ctx());
        let anonymous = ScopeContext {
            subject_tenant_id: OTHER_TENANT,
            subject_id: OTHER_SUBJECT,
            peer: PEER,
        };
        assert_eq!(
            key,
            resolve_scope_key(&l, &point(UPSTREAM), &anonymous),
            "no subject identity steers an ip counter"
        );
        let other_peer = ScopeContext {
            peer: OTHER_PEER,
            ..ctx()
        };
        assert_ne!(key, resolve_scope_key(&l, &point(UPSTREAM), &other_peer));
        assert!(key.contains(&PEER.to_string()));
        let ipv6 = ScopeContext {
            peer: IPV6_PEER,
            ..ctx()
        };
        let key = resolve_scope_key(&l, &point(UPSTREAM), &ipv6);
        assert!(key.contains("2001:db8::1"), "the canonical textual form");
    }

    #[test]
    fn the_route_scope_keys_on_the_upstream_route_pair() {
        let l = limit(&with_scope(block(10, WINDOW_MINUTE), SCOPE_ROUTE));
        let key_a = resolve_scope_key(
            &l,
            &routed(UPSTREAM, route(ROUTE_A, UPSTREAM, None)),
            &ctx(),
        );
        let key_b = resolve_scope_key(
            &l,
            &routed(UPSTREAM, route(ROUTE_B, UPSTREAM, None)),
            &ctx(),
        );
        assert_ne!(key_a, key_b, "two routes of one upstream never share");
        let unmatched = resolve_scope_key(&l, &point(UPSTREAM), &ctx());
        assert_ne!(
            key_a, unmatched,
            "the pair is not the upstream's own counter"
        );
        let expected = format!("{KEY_PREFIX}upstream:{UPSTREAM}:route:{UPSTREAM}:minute");
        assert_eq!(
            unmatched, expected,
            "no matched route degenerates to the id"
        );
    }

    #[test]
    fn the_route_s_own_block_is_attributed_to_the_route() {
        let route_block = with_cost(block(10, WINDOW_MINUTE), 5);
        let l = limit(&route_block);
        let key = resolve_scope_key(
            &l,
            &routed(
                UPSTREAM,
                route(ROUTE_A, UPSTREAM, Some(route_block.clone())),
            ),
            &ctx(),
        );
        assert_eq!(key.split(':').nth(2), Some(RESOURCE_TYPE_ROUTE));
        assert_eq!(key.split(':').nth(3), Some(ROUTE_A.to_string().as_str()));
    }

    /// The tie the value comparison decides: a route block that is value-equal
    /// to the effective limit reads as the block the limit came from, because
    /// the merge the limit is a value of lets the matched route's block enter
    /// after every upstream tier (`inst-me-09`), so the equal values are the
    /// route's own. The counter is attributed to the route.
    #[test]
    fn a_route_block_value_equal_to_the_effective_limit_is_attributed_to_the_route() {
        let equal = block(10, WINDOW_MINUTE);
        let l = limit(&equal);
        let key = resolve_scope_key(
            &l,
            &routed(UPSTREAM, route(ROUTE_A, UPSTREAM, Some(equal.clone()))),
            &ctx(),
        );

        assert_eq!(
            l,
            limit(&equal),
            "the fixture's effective limit is the value-equal block's"
        );
        assert_eq!(key.split(':').nth(2), Some(RESOURCE_TYPE_ROUTE));
        assert_eq!(key.split(':').nth(3), Some(ROUTE_A.to_string().as_str()));
    }

    #[test]
    fn a_block_that_is_not_the_route_s_is_attributed_to_the_upstream() {
        let effective = block(10, WINDOW_MINUTE);
        let l = limit(&effective);
        let stricter = with_cost(block(10, WINDOW_MINUTE), 5);
        let key = resolve_scope_key(
            &l,
            &routed(UPSTREAM, route(ROUTE_A, UPSTREAM, Some(stricter))),
            &ctx(),
        );
        assert_eq!(key.split(':').nth(2), Some(RESOURCE_TYPE_UPSTREAM));
        let without_block = resolve_scope_key(
            &l,
            &routed(UPSTREAM, route(ROUTE_A, UPSTREAM, None)),
            &ctx(),
        );
        assert_eq!(key, without_block);
    }

    #[test]
    fn two_limits_differing_only_in_window_never_share_a_counter() {
        let per_second = limit(&block(10, WINDOW_SECOND));
        let per_minute = limit(&block(10, WINDOW_MINUTE));
        assert_ne!(
            resolve_scope_key(&per_second, &point(UPSTREAM), &ctx()),
            resolve_scope_key(&per_minute, &point(UPSTREAM), &ctx()),
        );
    }

    #[test]
    fn no_caller_supplied_content_enters_the_key() {
        for scope in [
            SCOPE_GLOBAL,
            SCOPE_TENANT,
            SCOPE_USER,
            SCOPE_IP,
            SCOPE_ROUTE,
        ] {
            let configured = with_scope(block(10, WINDOW_MINUTE), scope);
            let l = limit(&configured);
            let key = resolve_scope_key(
                &l,
                &routed(UPSTREAM, route(ROUTE_A, UPSTREAM, None)),
                &ctx(),
            );
            assert_eq!(key.split(':').count(), 7, "{scope} adds no component");
        }
    }

    #[test]
    fn the_sharing_mode_changes_nothing_the_enforcement_reads() {
        // Two blocks that differ only in the sharing mode resolve the same
        // enforcement parameters, the same counter key and the same decision:
        // the limit enforced is the one the merge handed over, exactly as it
        // was received, and no sharing mode is applied anywhere in §2 or §3.
        let private = limit(&with_sharing(block(10, WINDOW_MINUTE), SHARING_PRIVATE));
        let enforced = limit(&with_sharing(block(10, WINDOW_MINUTE), SHARING_ENFORCE));
        assert_eq!(
            private, enforced,
            "the sharing mode is a control-plane field, not an enforcement input"
        );
        assert_eq!(
            resolve_scope_key(&private, &point(UPSTREAM), &ctx()),
            resolve_scope_key(&enforced, &point(UPSTREAM), &ctx()),
            "the two blocks count into the same bucket"
        );

        let now = start();
        let mut private_bucket = full_bucket(&private, now);
        let mut enforced_bucket = full_bucket(&enforced, now);
        let private_decision = decide(&mut private_bucket, &private, now);
        let enforced_decision = decide(&mut enforced_bucket, &enforced, now);
        assert_eq!(private_decision, enforced_decision);
        assert_eq!(
            private_bucket.tokens, enforced_bucket.tokens,
            "the balances the two checks leave behind are the same"
        );
    }

    #[test]
    fn an_instant_earlier_than_the_last_update_injects_no_token() {
        // The only clock the check reads is the monotonic one, and an interval
        // that is not monotonic saturates to zero: no token is injected, and
        // the check that finds nothing to pay the cost refuses.
        let now = start();
        let l = limit(&block(100, WINDOW_SECOND));
        let mut b = full_bucket(&l, now);
        b.tokens = 0.0;
        b.last_update = now + Duration::from_secs(30);
        refill(&mut b, now);
        assert!(
            near(b.tokens, 0.0),
            "an instant before the last update moves no balance"
        );
        assert_eq!(
            b.last_update, now,
            "the bucket adopts the instant the check observed"
        );
        assert!(
            !decide(&mut b, &l, now).acquired,
            "no token to pay the cost"
        );
        assert_eq!(b.last_update, now);
    }

    #[test]
    fn a_budget_block_is_not_a_field_the_enforcement_could_read() {
        // The `budget` block is not part of the validated field set, so a
        // payload cannot carry one and the enforcement has no budget to read:
        // this feature registers no route, owns no management endpoint and
        // touches only the 429 enforcement of the proxied request.
        let error = serde_json::from_value::<RateLimitConfig>(serde_json::json!({
            "sustained": { "rate": 10 },
            "budget": { "capacity": 5 }
        }))
        .expect_err("the block rejects the field it does not declare");
        assert!(
            error.to_string().contains("unknown field"),
            "deny_unknown_fields must reject the payload: {error}"
        );

        let declared: RateLimitConfig =
            serde_json::from_value(serde_json::json!({ "sustained": { "rate": 10 } }))
                .expect("the block this release validates");
        let l = EffectiveRateLimit::from_merged(&declared).expect("the block carries a rate");
        assert_eq!(
            l.burst_capacity, 10,
            "the burst is the sustained rate, and no budget block contributes to it"
        );
    }
}
