//! Rate-limit and circuit-breaker state of the `oagw` gear.
//!
//! Realizes `cpt-cf-oagw-dod-rate-limit-entities`: the four enforcement types
//! DECOMPOSITION §2.6 assigns `cpt-cf-oagw-feature-rate-limiting` —
//! [`TokenBucket`], [`RateLimiterRegistry`], [`BudgetAllocation`], and
//! [`CircuitBreakerState`] — declared once, in the domain layer, free of
//! transport and persistence types, referencing `RateLimitConfig` of
//! `cpt-cf-oagw-feature-gear-foundation` and `EffectiveRateLimit` of
//! `cpt-cf-oagw-feature-hierarchical-config` rather than redeclaring either,
//! and consuming the foundation's [`crate::domain::error::DomainError`]
//! catalogue for every answer it produces.
//!
//! The routines the feature's CDSL §3 states are here too, because they are
//! functions over this state and over the clock the caller hands them:
//! `cpt-cf-oagw-algo-effective-limit-fold` ([`fold`]),
//! `cpt-cf-oagw-algo-token-bucket` ([`token_bucket`]), and
//! `cpt-cf-oagw-algo-sliding-window` ([`sliding_window`]). The arithmetic is
//! integer throughout — milli-tokens for the refill a sub-second rate needs —
//! so a comparison never depends on a float rounding mode.
//!
//! Every instant the state holds is a [`std::time::Instant`]: the monotonic
//! clock the §1.4 assumption names. A monotonic instant cannot be formatted,
//! which is why the `X-RateLimit-Reset` header is derived at the API layer
//! from the wall clock and not here.

use std::time::{Duration, Instant};

use crate::domain::effective::EffectiveRateLimit;
use crate::domain::upstream::{
    Algorithm, RateLimitConfig, RateLimitScope, Strategy, Sustained, Window,
};

/// The breaker trips on the fifth failed attempt inside its rolling window,
/// which is the threshold PRD §6.1 states and DESIGN §4.7(1) defers the
/// configuration of (§1.5).
pub const BREAKER_FAILURE_THRESHOLD: usize = 5;

/// The rolling window the breaker counts failures inside, which PRD §6.1
/// states as 30 seconds.
pub const BREAKER_FAILURE_WINDOW: Duration = Duration::from_secs(30);

/// The interval an `open` machine serves before it admits one probe, a named
/// constant of this feature with no configuration surface and no sourced value
/// (§1.5).
pub const BREAKER_OPEN_INTERVAL: Duration = Duration::from_secs(30);

/// The count bound of the `queue` strategy's in-process queue, a named
/// constant with no configuration surface and no sourced value (§1.5).
pub const QUEUE_CAPACITY: usize = 64;

/// The wait bound of the `queue` strategy: a queued request that outwaits it
/// is answered 429 and charged nothing (§1.5).
pub const QUEUE_WAIT: Duration = Duration::from_millis(500);

/// The scale the bucket arithmetic works in: one token is this many units, so
/// a rate of one token per day still refills a visible amount per second.
const TOKEN_SCALE: u128 = 1_000;

/// The length of a window literal, in milliseconds.
#[must_use]
pub fn window_millis(window: Option<Window>) -> u128 {
    const SECOND: u128 = 1_000;
    match window {
        Some(Window::Minute) => 60 * SECOND,
        Some(Window::Hour) => 60 * 60 * SECOND,
        Some(Window::Day) => 24 * 60 * 60 * SECOND,
        Some(Window::Second) | None => SECOND,
    }
}

/// The sustained rate expressed per day, which is the common scale
/// `cpt-cf-oagw-algo-field-family-merge` normalizes to and the fold compares
/// on.
#[must_use]
pub fn per_common_scale(sustained: &Sustained) -> u64 {
    let millis = window_millis(sustained.window);
    let per_day = u128::from(sustained.rate) * 24 * 60 * 60 * 1_000;
    u64::try_from(per_day / millis).unwrap_or(u64::MAX)
}

/// The refill rate of a bucket, in milli-tokens per millisecond.
#[must_use]
fn refill_per_milli(sustained: &Sustained) -> u128 {
    let millis = window_millis(sustained.window);
    let per_window = u128::from(sustained.rate) * TOKEN_SCALE;
    // A rate of at least 1 per the widest window still yields a non-zero
    // milli-token amount per millisecond at this scale.
    per_window / millis
}

// @cpt-dod:cpt-cf-oagw-dod-rate-limit-entities:p1

/// One token bucket held for one counter key, the algorithm ADR 0003 selects
/// as the default.
///
/// `tokens` is held in milli-tokens so a refill a sub-second rate produces is
/// never lost to truncation; `capacity` and the comparisons stay in whole
/// tokens, which is the currency the configuration and the headers report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenBucket {
    /// The tokens the bucket holds, in milli-tokens.
    tokens_milli: u128,
    /// The ceiling the refill is capped at, in whole tokens.
    pub capacity: u64,
    /// The refill rate, in milli-tokens per millisecond.
    refill_per_milli: u128,
    /// The instant the bucket was last read or refilled at.
    pub updated: Instant,
}

impl TokenBucket {
    /// Initializes an absent bucket at full capacity, so a first burst is
    /// admitted up to `burst.capacity` (§1.5).
    #[must_use]
    pub fn full(capacity: u64, sustained: &Sustained, now: Instant) -> Self {
        Self {
            tokens_milli: u128::from(capacity) * TOKEN_SCALE,
            capacity,
            refill_per_milli: refill_per_milli(sustained),
            updated: now,
        }
    }

    /// The whole tokens the bucket holds after the last refill.
    #[must_use]
    pub fn tokens(&self) -> u64 {
        u64::try_from(self.tokens_milli / TOKEN_SCALE).unwrap_or(u64::MAX)
    }

    /// Adds the refill the elapsed time earns, capped at the capacity, and
    /// stamps the update instant.
    ///
    /// A clock that moved backwards between two readings is no elapsed time at
    /// all rather than a negative refill, so a clock adjustment adds no tokens
    /// and removes none (§1.4).
    pub fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.updated);
        self.updated = now;
        if elapsed.is_zero() {
            return;
        }
        let millis = elapsed.as_millis();
        let earned = millis.saturating_mul(self.refill_per_milli);
        let ceiling = u128::from(self.capacity) * TOKEN_SCALE;
        self.tokens_milli = (self.tokens_milli + earned).min(ceiling);
    }

    /// The whole-second delay until the bucket holds `cost`, rounded up.
    ///
    /// A `cost` the bucket cannot hold even when full saturates the shortfall,
    /// which the refusal answers with the minimum delay of one second.
    #[must_use]
    pub fn delay_for(&self, cost: u64) -> u64 {
        let shortfall = (u128::from(cost) * TOKEN_SCALE).saturating_sub(self.tokens_milli);
        let millis = shortfall * TOKEN_SCALE / self.refill_per_milli / TOKEN_SCALE;
        let seconds = millis / 1_000;
        // Round up to a whole second, and never answer zero for a delay the
        // bucket does need.
        let whole = if millis.is_multiple_of(1_000) { seconds } else { seconds + 1 };
        u64::try_from(whole.max(1)).unwrap_or(u64::MAX)
    }

    /// The whole-second delay until the bucket is full again, rounded up, which
    /// is the `X-RateLimit-Reset` offset the header reports.
    #[must_use]
    pub fn full_in_seconds(&self) -> u64 {
        let ceiling = u128::from(self.capacity) * TOKEN_SCALE;
        if self.tokens_milli >= ceiling {
            return 0;
        }
        let shortfall = ceiling - self.tokens_milli;
        let millis = shortfall / self.refill_per_milli;
        let seconds = millis / 1_000;
        let whole = if millis.is_multiple_of(1_000) { seconds } else { seconds + 1 };
        u64::try_from(whole).unwrap_or(u64::MAX)
    }
}

/// One sliding window held for one counter key, the algorithm ADR 0003 prefers
/// where strict rate enforcement matters more than burst tolerance.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SlidingWindow {
    /// Every charge the window still holds, each with the instant it was
    /// recorded at, oldest first.
    charges: Vec<(Instant, u64)>,
}

impl SlidingWindow {
    /// Drops every charge whose instant falls outside the window length.
    pub fn expire(&mut self, length: Duration, now: Instant) {
        self.charges
            .retain(|(at, _)| now.saturating_duration_since(*at) < length);
    }

    /// The sum of the charges the window still holds.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.charges.iter().map(|(_, cost)| cost).sum()
    }

    /// Records one charge at the current instant.
    pub fn record(&mut self, cost: u64, now: Instant) {
        self.charges.push((now, cost));
    }

    /// The whole-second delay until the window holds `cost`: the time until
    /// enough of the oldest charges age out, rounded up.
    #[must_use]
    pub fn delay_for(&self, cost: u64, rate: u64, length: Duration, now: Instant) -> u64 {
        let mut needed = u128::from(self.total() + cost) - u128::from(rate);
        for (at, charge) in &self.charges {
            let leaving = u128::from(*charge);
            if leaving >= needed {
                let waited = now.saturating_duration_since(*at);
                let remaining = length.saturating_sub(waited);
                let seconds = remaining.as_secs();
                return if remaining.subsec_nanos() == 0 {
                    seconds.max(1)
                } else {
                    seconds + 1
                };
            }
            needed -= leaving;
        }
        1
    }

    /// The whole-second delay until the oldest charge ages out of the window,
    /// rounded up, which is the `X-RateLimit-Reset` offset the header reports.
    /// A window that holds no charge is already at its allowance.
    #[must_use]
    pub fn oldest_ages_out_in_seconds(&self, length: Duration, now: Instant) -> u64 {
        let Some((oldest, _)) = self.charges.first() else {
            return 0;
        };
        let waited = now.saturating_duration_since(*oldest);
        let remaining = length.saturating_sub(waited);
        let seconds = remaining.as_secs();
        if remaining.subsec_nanos() == 0 {
            seconds
        } else {
            seconds + 1
        }
    }
}

/// The verdict one acquisition attempt produced.
///
/// A refused attempt records no charge, so the state it reports is the state
/// the next attempt is compared against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcquireOutcome {
    /// Whether the counter covered the request's `cost`.
    pub admitted: bool,
    /// The amount the counter holds after the attempt, in the currency the
    /// effective algorithm reports.
    pub remaining: u64,
    /// The whole-second delay until the counter holds the `cost`, which is the
    /// `Retry-After` a refusal answers with; zero on an admission.
    pub delay_seconds: u64,
    /// The whole-second delay until the counter reaches its capacity again,
    /// which is the `X-RateLimit-Reset` a refusal reports as an offset from the
    /// wall clock; zero on an admission.
    pub reset_seconds: u64,
}

/// The layer whose `rate_limit` the effective limit came from, which is the
/// resource the counter key's prefix names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitLayer {
    /// The upstream layer won, so the prefix names the resolved upstream.
    Upstream,
    /// The route layer won, so the prefix names the matched route.
    Route,
}

/// The limit one request is enforced against, the fold's output.
///
/// The sustained rate is the minimum of the visible layer rates, reported in
/// the winning layer's window; the capacity is the minimum of the visible
/// `burst.capacity` values or the sustained rate when no layer declares one;
/// and the four members that carry no merge come from the last layer that
/// declares each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveLimit {
    /// The effective sustained rate and its window.
    pub sustained: Sustained,
    /// The effective burst capacity.
    pub burst_capacity: u64,
    /// The algorithm the counter runs.
    pub algorithm: Algorithm,
    /// The counter scope.
    pub scope: RateLimitScope,
    /// The behaviour when the limit is exceeded.
    pub strategy: crate::domain::upstream::Strategy,
    /// The tokens one request costs.
    pub cost: u64,
    /// The layer whose `rate_limit` the sustained rate came from.
    pub layer: LimitLayer,
}

impl EffectiveLimit {
    /// The default capacity: the sustained rate, which is the default ADR
    /// 0003's field table declares for a `burst.capacity` no layer states.
    #[must_use]
    pub fn capacity(&self) -> u64 {
        self.burst_capacity
    }
}

/// The layer values the resolution produced, in the order the fold applies
/// them.
///
/// The upstream layer comes first and the route layer second; the tenant
/// layer's contributions arrive folded inside both by
/// `cpt-cf-oagw-algo-field-family-merge`, so the fold walks no chain and
/// applies no per-field merge strategy of its own.
#[derive(Debug, Clone, Copy, Default)]
pub struct LimitLayers<'a> {
    /// The upstream layer value, ancestors included.
    pub upstream: Option<&'a EffectiveRateLimit>,
    /// The route layer value, ancestors included.
    pub route: Option<&'a EffectiveRateLimit>,
}

// @cpt-dod:cpt-cf-oagw-dod-rate-limit-hierarchy:p1

/// Folds the resolved layers into the one limit a request is enforced against.
///
/// Returns `None` when no layer carries a `rate_limit` at all, which is the
/// outcome `cpt-cf-oagw-flow-rate-limit-check` enforces nothing on: an
/// unconfigured upstream is not silently limited by a default it never
/// declared.
#[must_use]
pub fn fold(layers: &LimitLayers<'_>) -> Option<EffectiveLimit> {
    // @cpt-begin:cpt-cf-oagw-algo-effective-limit-fold:p1:inst-fold-collect
    // Every layer value the resolution produced that a `private` ancestor did
    // not withhold, in the order upstream, then route, then tenant. The merge
    // withheld the private ones before the value arrived, so what is collected
    // here is what is visible.
    let visible: Vec<(&RateLimitConfig, LimitLayer)> = [
        layers.upstream.map(|limit| (&limit.rate_limit, LimitLayer::Upstream)),
        layers.route.map(|limit| (&limit.rate_limit, LimitLayer::Route)),
    ]
    .into_iter()
    .flatten()
    .filter(|(limit, _)| limit.sustained.is_some())
    .collect();
    // @cpt-end:cpt-cf-oagw-algo-effective-limit-fold:p1:inst-fold-collect

    // @cpt-begin:cpt-cf-oagw-algo-effective-limit-fold:p1:inst-fold-none-if
    if visible.is_empty() {
        // @cpt-begin:cpt-cf-oagw-algo-effective-limit-fold:p1:inst-fold-none
        // The no-limit outcome: no layer carries a `rate_limit`, so the check
        // enforces nothing and charges nothing.
        return None;
        // @cpt-end:cpt-cf-oagw-algo-effective-limit-fold:p1:inst-fold-none
    }
    // @cpt-end:cpt-cf-oagw-algo-effective-limit-fold:p1:inst-fold-none-if

    // @cpt-begin:cpt-cf-oagw-algo-effective-limit-fold:p1:inst-fold-rate
    // The minimum of the visible sustained rates, which the merge already
    // normalized to one scale and reported in the winning layer's window; the
    // comparison here is on that same scale, and the winner's own window is
    // what the effective limit reports.
    let mut winner: Option<(&RateLimitConfig, LimitLayer, &Sustained)> = None;
    for (limit, layer) in &visible {
        let Some(sustained) = limit.sustained.as_ref() else {
            continue;
        };
        let closer = match winner {
            None => true,
            Some((_, _, held)) => per_common_scale(sustained) < per_common_scale(held),
        };
        if closer {
            winner = Some((limit, *layer, sustained));
        }
    }
    let (_, layer, sustained) = winner?;
    // @cpt-end:cpt-cf-oagw-algo-effective-limit-fold:p1:inst-fold-rate

    // @cpt-begin:cpt-cf-oagw-algo-effective-limit-fold:p1:inst-fold-burst
    // The minimum of the visible `burst.capacity` values under the same mode
    // gate, which is the merge ADR 0003's Example 1 performs beside the
    // sustained one; the default is the sustained rate.
    let burst_capacity = visible
        .iter()
        .filter_map(|(limit, _)| limit.burst.as_ref().map(|burst| burst.capacity))
        .min()
        .unwrap_or(sustained.rate);
    // @cpt-end:cpt-cf-oagw-algo-effective-limit-fold:p1:inst-fold-burst

    // @cpt-begin:cpt-cf-oagw-algo-effective-limit-fold:p1:inst-fold-members
    // The four members that carry no merge come from the last layer that
    // declares each, in the upstream, then route, then tenant order the
    // layers are walked in, so the route layer's declaration prevails over the
    // upstream layer's; a member no layer declares takes the default ADR
    // 0003's field table declares.
    let mut from_last_declaring = visible.iter().rev();
    // @cpt-end:cpt-cf-oagw-algo-effective-limit-fold:p1:inst-fold-members

    // @cpt-begin:cpt-cf-oagw-algo-effective-limit-fold:p1:inst-fold-return
    // RETURN the effective limit and its four carried members.
    Some(EffectiveLimit {
        sustained: sustained.clone(),
        burst_capacity,
        algorithm: from_last_declaring
            .clone()
            .find_map(|(limit, _)| limit.algorithm)
            .unwrap_or(Algorithm::TokenBucket),
        scope: from_last_declaring
            .clone()
            .find_map(|(limit, _)| limit.scope)
            .unwrap_or(RateLimitScope::Tenant),
        strategy: from_last_declaring
            .clone()
            .find_map(|(limit, _)| limit.strategy)
            .unwrap_or(Strategy::Reject),
        cost: from_last_declaring
            .find_map(|(limit, _)| limit.cost)
            .unwrap_or(1),
        layer,
    })
    // @cpt-end:cpt-cf-oagw-algo-effective-limit-fold:p1:inst-fold-return
}

// @cpt-dod:cpt-cf-oagw-dod-rate-limit-algorithms:p1

/// Runs one acquisition against a token bucket.
///
/// The refill runs first, so a bucket that has been idle is read at its
/// refilled level and a refusal is measured against the tokens the bucket
/// holds now.
pub fn token_bucket(bucket: &mut TokenBucket, cost: u64, now: Instant) -> AcquireOutcome {
    // @cpt-begin:cpt-cf-oagw-algo-token-bucket:p1:inst-tb-init-if
    // @cpt-begin:cpt-cf-oagw-algo-token-bucket:p1:inst-tb-init
    // An absent bucket is initialized at full capacity by the registry that
    // holds it, which is this step read as the state it hands over; the bucket
    // this routine receives is therefore never absent, and the refill below is
    // its first act.
    // @cpt-end:cpt-cf-oagw-algo-token-bucket:p1:inst-tb-init
    // @cpt-end:cpt-cf-oagw-algo-token-bucket:p1:inst-tb-init-if

    // @cpt-begin:cpt-cf-oagw-algo-token-bucket:p1:inst-tb-refill
    // Refill: the elapsed time since the bucket's last update multiplied by
    // the refill rate, capped at the capacity, and stamped.
    bucket.refill(now);
    // @cpt-end:cpt-cf-oagw-algo-token-bucket:p1:inst-tb-refill

    // @cpt-begin:cpt-cf-oagw-algo-token-bucket:p1:inst-tb-compare
    // Compare the refilled tokens against the request's `cost`.
    let covered = bucket.tokens_milli >= u128::from(cost) * TOKEN_SCALE;
    // @cpt-end:cpt-cf-oagw-algo-token-bucket:p1:inst-tb-compare

    // @cpt-begin:cpt-cf-oagw-algo-token-bucket:p1:inst-tb-allow-if
    if covered {
        // @cpt-begin:cpt-cf-oagw-algo-token-bucket:p1:inst-tb-allow
        // Subtract the `cost` and report the admission and the tokens
        // remaining.
        bucket.tokens_milli -= u128::from(cost) * TOKEN_SCALE;
        return AcquireOutcome {
            admitted: true,
            remaining: bucket.tokens(),
            delay_seconds: 0,
            reset_seconds: bucket.full_in_seconds(),
        };
        // @cpt-end:cpt-cf-oagw-algo-token-bucket:p1:inst-tb-allow
    }
    // @cpt-end:cpt-cf-oagw-algo-token-bucket:p1:inst-tb-allow-if

    // @cpt-begin:cpt-cf-oagw-algo-token-bucket:p1:inst-tb-allow-else
    // The ELSE of the comparison: the refusal, the tokens remaining, and the
    // delay as the shortfall against the `cost` divided by the refill rate,
    // rounded up to a whole second. A refused request records no charge.
    // @cpt-begin:cpt-cf-oagw-algo-token-bucket:p1:inst-tb-refuse
    let delay = bucket.delay_for(cost);
    let reset = bucket.full_in_seconds();
    // @cpt-end:cpt-cf-oagw-algo-token-bucket:p1:inst-tb-refuse
    // @cpt-end:cpt-cf-oagw-algo-token-bucket:p1:inst-tb-allow-else
    AcquireOutcome {
        admitted: false,
        remaining: bucket.tokens(),
        delay_seconds: delay,
        reset_seconds: reset,
    }
}

/// Runs one acquisition against a token bucket whose burst reserve is
/// withheld, which is the `degrade` strategy's posture (§1.5).
///
/// The reserve is the capacity above the sustained rate. The counter the
/// bucket holds is shared with every strategy the configuration names, so the
/// reserve is withheld for the attempt and never written back into the bucket:
/// a request that arrives while the reserve is withheld reads no more than the
/// sustained rate, and a token it spends is spent from the bucket itself.
pub fn token_bucket_capped(
    bucket: &mut TokenBucket,
    cost: u64,
    ceiling: u64,
    now: Instant,
) -> AcquireOutcome {
    // The same refill and comparison as the uncapped acquisition, read through
    // the reserve the ceiling withholds: the burst capacity above the
    // sustained rate stays in the bucket and no acquisition may spend it, so
    // the allowance the degraded posture leaves is the sustained rate and the
    // reserve is what remains after it.
    bucket.refill(now);
    let reserve_milli = u128::from(bucket.capacity.saturating_sub(ceiling)) * TOKEN_SCALE;
    let effective = bucket.tokens_milli.saturating_sub(reserve_milli);
    let allowed = u64::try_from(effective / TOKEN_SCALE).unwrap_or(u64::MAX);
    if effective >= u128::from(cost) * TOKEN_SCALE {
        bucket.tokens_milli -= u128::from(cost) * TOKEN_SCALE;
        return AcquireOutcome {
            admitted: true,
            remaining: allowed,
            delay_seconds: 0,
            reset_seconds: bucket.full_in_seconds(),
        };
    }
    AcquireOutcome {
        admitted: false,
        remaining: allowed,
        delay_seconds: bucket.delay_for(cost),
        reset_seconds: bucket.full_in_seconds(),
    }
}

/// Runs one acquisition against a sliding window.
pub fn sliding_window(
    window: &mut SlidingWindow,
    cost: u64,
    rate: u64,
    length: Duration,
    now: Instant,
) -> AcquireOutcome {
    // @cpt-begin:cpt-cf-oagw-algo-sliding-window:p1:inst-sw-expire
    // Drop every charge recorded outside the window length, which is the
    // conversion of the `second`, `minute`, `hour`, and `day` literals the
    // shipped schema enumerates.
    window.expire(length, now);
    // @cpt-end:cpt-cf-oagw-algo-sliding-window:p1:inst-sw-expire

    // @cpt-begin:cpt-cf-oagw-algo-sliding-window:p1:inst-sw-sum
    // Sum the charges that remain.
    let total = window.total();
    // @cpt-end:cpt-cf-oagw-algo-sliding-window:p1:inst-sw-sum

    // @cpt-begin:cpt-cf-oagw-algo-sliding-window:p1:inst-sw-allow-if
    if total + cost <= rate {
        // @cpt-begin:cpt-cf-oagw-algo-sliding-window:p1:inst-sw-allow
        // Record the `cost` at the current instant and report the admission
        // and the new total.
        window.record(cost, now);
        return AcquireOutcome {
            admitted: true,
            remaining: rate - (total + cost),
            delay_seconds: 0,
            reset_seconds: window.oldest_ages_out_in_seconds(length, now),
        };
        // @cpt-end:cpt-cf-oagw-algo-sliding-window:p1:inst-sw-allow
    }
    // @cpt-end:cpt-cf-oagw-algo-sliding-window:p1:inst-sw-allow-if

    // @cpt-begin:cpt-cf-oagw-algo-sliding-window:p1:inst-sw-allow-else
    // The ELSE of the comparison: the refusal, the current total, and the
    // delay as the time until the oldest recorded charge ages out enough to
    // admit the `cost`. A refused request records no charge and so never
    // extends the window against itself.
    // @cpt-begin:cpt-cf-oagw-algo-sliding-window:p1:inst-sw-refuse
    let delay = window.delay_for(cost, rate, length, now);
    // @cpt-end:cpt-cf-oagw-algo-sliding-window:p1:inst-sw-refuse
    // @cpt-end:cpt-cf-oagw-algo-sliding-window:p1:inst-sw-allow-else
    AcquireOutcome {
        admitted: false,
        remaining: rate - total,
        delay_seconds: delay,
        reset_seconds: window.oldest_ages_out_in_seconds(length, now),
    }
}

/// The budget mode a parent configuration declares, which ADR 0003 gives three
/// of.
///
/// No written configuration can carry a `budget` member (§1.5), so the modes
/// and their arithmetic are exercised at the domain layer and through this
/// routine's colocated tests, and they become reachable from a written
/// configuration only when a schema revision admits the member.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetMode {
    /// Tracks nothing: the mode ADR 0003 declares as the default for a leaf
    /// tenant.
    Unlimited,
    /// Gives each child a fixed slice of the parent's budget.
    Allocated,
    /// Lets the children draw on the parent's total first-come-first-served,
    /// with no individual guarantee.
    Shared,
}

/// The budget object a parent configuration declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetAllocation {
    /// The parent's total, which the shipped field table sets a minimum of 1.
    pub total: u64,
    /// The ratio the children's sum may reach, which the same table sets a
    /// minimum of 1.0; held scaled by 100 so the arithmetic stays integer, so
    /// `150` is a ratio of 1.5.
    pub overcommit_ratio_percent: u64,
}

/// The outcome of one budget validation or charge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetOutcome {
    /// No tracking performed and nothing to validate.
    Unlimited,
    /// The children's sum fits the parent's ceiling.
    Accepted {
        /// The sum of the declared allocations and the candidate.
        allocated: u64,
        /// Whether the sum exceeds the parent's `total` while still fitting
        /// the ratio's ceiling, which is the warning ADR 0003's worked
        /// arithmetic shows for a ratio above 1.0.
        over_total: bool,
    },
    /// The sum exceeds the parent's ceiling.
    Rejected {
        /// The sum that was rejected.
        allocated: u64,
    },
    /// The charge was taken from the parent's pool.
    Shared {
        /// The amount the pool still holds after the charge.
        remaining: u64,
    },
}

/// Validates a child allocation against its parent's budget, or charges one
/// request against a `shared` pool.
///
/// A parent with no budget at all is treated as `unlimited` rather than as a
/// rejection, because a mode that tracks nothing cannot be exceeded; the
/// write path answers a [`BudgetOutcome::Rejected`] 400 through the
/// foundation's `ValidationError` variant.
#[must_use]
pub fn allocate_budget(
    parent: Option<BudgetAllocation>,
    mode: BudgetMode,
    pool_remaining: u64,
    children_sum: u64,
    candidate: u64,
) -> BudgetOutcome {
    // A parent whose budget is absent while a child declares an allocation is
    // a configuration the merged resolution would not have produced, and the
    // error handling of this routine treats it as `unlimited` rather than as a
    // rejection, because a mode that tracks nothing cannot be exceeded.
    let tracked = mode != BudgetMode::Unlimited && parent.is_some();

    // @cpt-begin:cpt-cf-oagw-algo-budget-allocate:p1:inst-bud-unlimited-if
    if !tracked {
        // @cpt-begin:cpt-cf-oagw-algo-budget-allocate:p1:inst-bud-unlimited
        // RETURN the no-tracking outcome, no validation performed, which is
        // the mode ADR 0003 declares as the default for a leaf tenant.
        return BudgetOutcome::Unlimited;
        // @cpt-end:cpt-cf-oagw-algo-budget-allocate:p1:inst-bud-unlimited
    }
    // @cpt-end:cpt-cf-oagw-algo-budget-allocate:p1:inst-bud-unlimited-if

    // @cpt-begin:cpt-cf-oagw-algo-budget-allocate:p1:inst-bud-allocated-if
    if mode == BudgetMode::Allocated {
        let Some(parent) = parent else {
            return BudgetOutcome::Unlimited;
        };
        // @cpt-begin:cpt-cf-oagw-algo-budget-allocate:p1:inst-bud-sum
        // Sum the children's declared allocations together with the one under
        // consideration.
        let sum = children_sum.saturating_add(candidate);
        // @cpt-end:cpt-cf-oagw-algo-budget-allocate:p1:inst-bud-sum

        // @cpt-begin:cpt-cf-oagw-algo-budget-allocate:p1:inst-bud-over-if
        let ceiling = parent.total.saturating_mul(parent.overcommit_ratio_percent) / 100;
        if sum > ceiling {
            // @cpt-begin:cpt-cf-oagw-algo-budget-allocate:p1:inst-bud-over
            // RETURN the rejection, which the write path answers 400.
            return BudgetOutcome::Rejected { allocated: sum };
            // @cpt-end:cpt-cf-oagw-algo-budget-allocate:p1:inst-bud-over
        }
        // @cpt-end:cpt-cf-oagw-algo-budget-allocate:p1:inst-bud-over-if

        // @cpt-begin:cpt-cf-oagw-algo-budget-allocate:p1:inst-bud-over-else
        // The ELSE of the ceiling check: the acceptance, with the warning
        // recorded when the sum exceeds the parent's `total` but not the
        // ratio's ceiling, which is the outcome ADR 0003's worked arithmetic
        // shows for a ratio above 1.0.
        // @cpt-begin:cpt-cf-oagw-algo-budget-allocate:p1:inst-bud-accept
        return BudgetOutcome::Accepted {
            allocated: sum,
            over_total: sum > parent.total,
        };
        // @cpt-end:cpt-cf-oagw-algo-budget-allocate:p1:inst-bud-accept
        // @cpt-end:cpt-cf-oagw-algo-budget-allocate:p1:inst-bud-over-else
    }
    // @cpt-end:cpt-cf-oagw-algo-budget-allocate:p1:inst-bud-allocated-if

    // @cpt-begin:cpt-cf-oagw-algo-budget-allocate:p1:inst-bud-shared-if
    // The `shared` mode: the request's `cost` is charged against the parent's
    // pool counter with no per-child allocation validated, which is the
    // first-come-first-served behaviour ADR 0003 states for the mode.
    // @cpt-begin:cpt-cf-oagw-algo-budget-allocate:p1:inst-bud-shared
    // RETURN the amount the pool still holds.
    BudgetOutcome::Shared {
        remaining: pool_remaining.saturating_sub(candidate),
    }
    // @cpt-end:cpt-cf-oagw-algo-budget-allocate:p1:inst-bud-shared
    // @cpt-end:cpt-cf-oagw-algo-budget-allocate:p1:inst-bud-shared-if
}

/// The phase of one circuit-breaker machine, the one state machine this
/// feature owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BreakerPhase {
    /// The upstream is in rotation and every attempt is admitted.
    #[default]
    Closed,
    /// The upstream is out of rotation and every attempt is answered 503.
    Open,
    /// The open interval elapsed and one probe is in flight.
    HalfOpen,
}

/// One transition the breaker machine moved through, reported by the move
/// itself.
///
/// The pair is what `oagw_circuit_breaker_transitions_total` is incremented
/// with, read from the machine that made the move and never re-derived by the
/// reader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BreakerTransition {
    /// The phase the machine moved from.
    pub from: BreakerPhase,
    /// The phase the machine moved to.
    pub to: BreakerPhase,
}

/// The rolling window, the open stamp, and the probe bound of one upstream's
/// breaker, which is what the `oagw_circuit_breaker_state` gauge reports
/// against the upstream alias.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CircuitBreakerState {
    /// The phase the machine holds.
    pub phase: BreakerPhase,
    /// The instants the counted failures of the rolling window were recorded
    /// at, oldest first.
    pub failures: Vec<Instant>,
    /// The instant the current open interval began.
    pub open_since: Option<Instant>,
    /// The transitions the machine has reported since a reader last drained
    /// them, oldest first, bounded so a machine that moves between reads
    /// cannot grow the record without limit.
    reported: Vec<BreakerTransition>,
}

/// The most transitions one machine holds between two reads.
const BREAKER_REPORTED_CAPACITY: usize = 64;

impl Default for CircuitBreakerState {
    fn default() -> Self {
        Self {
            phase: BreakerPhase::Closed,
            failures: Vec::new(),
            open_since: None,
            reported: Vec::new(),
        }
    }
}

impl CircuitBreakerState {
    /// Moves the machine to a phase, reporting the move it made.
    ///
    /// A move to the phase the machine already holds is not a transition and
    /// is reported as none, so a reader that drains the log counts moves and
    /// not attempts.
    fn move_to(&mut self, to: BreakerPhase) {
        let from = self.phase;
        self.phase = to;
        if from != to && self.reported.len() < BREAKER_REPORTED_CAPACITY {
            self.reported.push(BreakerTransition { from, to });
        }
    }

    /// The transitions the machine reported since the last read, oldest first,
    /// draining them: a reader that takes the log takes the whole of it, so no
    /// transition is reported twice and none is held after its read.
    pub fn drain_reported(&mut self) -> Vec<BreakerTransition> {
        std::mem::take(&mut self.reported)
    }

    /// Whether the machine admits one attempt right now.
    ///
    /// An `open` machine whose interval has elapsed moves to `half_open` and
    /// grants the attempt that moved it the single probe; a `half_open`
    /// machine admits nothing further until that probe resolves, so every
    /// concurrent request for the same upstream is answered 503 (§1.5).
    pub fn admit(&mut self, now: Instant) -> bool {
        // @cpt-begin:cpt-cf-oagw-state-circuit-breaker:p1:inst-cb-probe
        // The open interval elapsed: the machine moves to `half_open`, admits
        // one probe — the attempt that asked — and no more.
        if self.phase == BreakerPhase::Open {
            let served = self
                .open_since
                .is_none_or(|since| now.saturating_duration_since(since) >= BREAKER_OPEN_INTERVAL);
            if served {
                self.move_to(BreakerPhase::HalfOpen);
                return true;
            }
            return false;
        }
        self.phase == BreakerPhase::Closed
        // @cpt-end:cpt-cf-oagw-state-circuit-breaker:p1:inst-cb-probe
    }

    /// The seconds remaining of the open interval, which the 503 answer
    /// carries as its `Retry-After`.
    #[must_use]
    pub fn retry_after_seconds(&self, now: Instant) -> u64 {
        match (self.phase, self.open_since) {
            (BreakerPhase::Open, Some(since)) => {
                let served = now.saturating_duration_since(since);
                BREAKER_OPEN_INTERVAL
                    .saturating_sub(served)
                    .as_secs()
                    .max(1)
            }
            _ => 1,
        }
    }

    /// Records one attempt outcome and returns the phase the machine holds.
    ///
    /// `succeeded` is whether the attempt produced an answer from the target
    /// at all; `counted` is whether that answer is one of the three rows the
    /// §1.5 deviation enumerates, the only rows evidence about the target's
    /// reachability is made of.
    pub fn count(&mut self, succeeded: bool, counted: bool, now: Instant) -> BreakerPhase {
        // @cpt-begin:cpt-cf-oagw-algo-breaker-count:p1:inst-brc-absent-if
        if !succeeded && !counted {
            // @cpt-begin:cpt-cf-oagw-algo-breaker-count:p1:inst-brc-absent
            // No evidence, no change: the breaker never opens on the absence
            // of a classification (§1.5). The one exception is the stall: a
            // `half_open` machine that never receives its probe's outcome
            // returns to `open` when the open interval elapses again counted
            // from the same stamp the transition read, so one lost
            // classification cannot hold the upstream at 503 until a restart.
            if self.phase == BreakerPhase::HalfOpen
                && let Some(since) = self.open_since
                && now.saturating_duration_since(since) >= 2 * BREAKER_OPEN_INTERVAL
            {
                self.move_to(BreakerPhase::Open);
            }
            return self.phase;
            // @cpt-end:cpt-cf-oagw-algo-breaker-count:p1:inst-brc-absent
        }
        // @cpt-end:cpt-cf-oagw-algo-breaker-count:p1:inst-brc-absent-if

        // @cpt-begin:cpt-cf-oagw-algo-breaker-count:p1:inst-brc-success-if
        if succeeded {
            // @cpt-begin:cpt-cf-oagw-algo-breaker-count:p1:inst-brc-success
            // Clear the failure count, and return a `half_open` machine to
            // `closed`: the probe earned the upstream its way back.
            self.failures.clear();
            if self.phase == BreakerPhase::HalfOpen {
                // @cpt-begin:cpt-cf-oagw-state-circuit-breaker:p1:inst-cb-recover
                // The probe attempt succeeded, so the machine returns to
                // `closed` and drops the open-interval stamp with it.
                self.move_to(BreakerPhase::Closed);
                self.open_since = None;
                // @cpt-end:cpt-cf-oagw-state-circuit-breaker:p1:inst-cb-recover
            }
            return self.phase;
            // @cpt-end:cpt-cf-oagw-algo-breaker-count:p1:inst-brc-success
        }
        // @cpt-end:cpt-cf-oagw-algo-breaker-count:p1:inst-brc-success-if

        // @cpt-begin:cpt-cf-oagw-algo-breaker-count:p1:inst-brc-fail-if
        // @cpt-begin:cpt-cf-oagw-algo-breaker-count:p1:inst-brc-record
        // Append the failure to the rolling window and drop the entries older
        // than the 30 seconds PRD §6.1 states.
        self.failures.push(now);
        self.failures
            .retain(|at| now.saturating_duration_since(*at) <= BREAKER_FAILURE_WINDOW);
        // @cpt-end:cpt-cf-oagw-algo-breaker-count:p1:inst-brc-record
        // @cpt-end:cpt-cf-oagw-algo-breaker-count:p1:inst-brc-fail-if

        // @cpt-begin:cpt-cf-oagw-algo-breaker-count:p1:inst-brc-trip-if
        if self.phase == BreakerPhase::Closed && self.failures.len() >= BREAKER_FAILURE_THRESHOLD
        {
            // @cpt-begin:cpt-cf-oagw-state-circuit-breaker:p1:inst-cb-trip
            // @cpt-begin:cpt-cf-oagw-algo-breaker-count:p1:inst-brc-trip
            // Move the machine to `open` and stamp the instant the open
            // interval began, which is the trip of PRD §6.1's threshold.
            self.move_to(BreakerPhase::Open);
            self.open_since = Some(now);
            // @cpt-end:cpt-cf-oagw-algo-breaker-count:p1:inst-brc-trip
            // @cpt-end:cpt-cf-oagw-state-circuit-breaker:p1:inst-cb-trip
        }
        // @cpt-end:cpt-cf-oagw-algo-breaker-count:p1:inst-brc-trip-if

        // @cpt-begin:cpt-cf-oagw-state-circuit-breaker:p1:inst-cb-stay-open
        // A further failure recorded while the machine is open neither
        // re-trips it into a new classification nor extends the interval it is
        // already serving: the stamp stays the instant the trip was recorded
        // at, and the next admission is measured from it.
        // @cpt-end:cpt-cf-oagw-state-circuit-breaker:p1:inst-cb-stay-open

        // @cpt-begin:cpt-cf-oagw-state-circuit-breaker:p1:inst-cb-reopen
        // A probe that failed, or a counted failure recorded while the machine
        // is already open: the machine returns to `open` and the open interval
        // restarts from its beginning. A further failure while the machine is
        // open neither re-trips it into a new classification nor extends the
        // interval it is already serving, because the stamp is the instant the
        // failure was recorded at and the next admission is measured from it.
        if self.phase == BreakerPhase::HalfOpen {
            self.move_to(BreakerPhase::Open);
            self.open_since = Some(now);
        }
        // @cpt-end:cpt-cf-oagw-state-circuit-breaker:p1:inst-cb-reopen

        // @cpt-begin:cpt-cf-oagw-state-circuit-breaker:p1:inst-cb-stall
        // A `half_open` machine whose probe outcome never arrives returns to
        // `open` when the open interval elapses again counted from the same
        // stamp the probe transition read, which the next [`Self::admit`]
        // enforces, so one lost classification is a re-probe and not a
        // permanent outage.
        // @cpt-end:cpt-cf-oagw-state-circuit-breaker:p1:inst-cb-stall

        // @cpt-begin:cpt-cf-oagw-algo-breaker-count:p1:inst-brc-else
        // The ELSE of the classification chain.
        // @cpt-begin:cpt-cf-oagw-algo-breaker-count:p1:inst-brc-ignore
        // A 4xx answer, an upstream error status passed through as
        // `DownstreamError`, and a 429 this feature produced are not evidence
        // about the target's reachability, so a `closed` machine that receives
        // one changes nothing (§1.5).
        // @cpt-end:cpt-cf-oagw-algo-breaker-count:p1:inst-brc-ignore
        // @cpt-end:cpt-cf-oagw-algo-breaker-count:p1:inst-brc-else

        // @cpt-begin:cpt-cf-oagw-algo-breaker-count:p1:inst-brc-return
        // RETURN the resulting state.
        self.phase
        // @cpt-end:cpt-cf-oagw-algo-breaker-count:p1:inst-brc-return
    }
}

/// The per-instance in-process registry ADR 0006 assigns to the Data Plane.
///
/// Every bucket, window, budget pool, and breaker machine the gear holds is
/// keyed under the `{resource_type}:{resource_id}` prefix ADR 0003's key
/// structure gives, so two upstreams limited at the same `scope` never share a
/// counter and a prefix drop has exactly one owner. Nothing here is persisted,
/// and the restart loss of a counter is accepted (§1.5).
#[derive(Debug, Default)]
pub struct RateLimiterRegistry {
    buckets: std::collections::HashMap<String, TokenBucket>,
    windows: std::collections::HashMap<String, SlidingWindow>,
    breakers: std::collections::HashMap<String, CircuitBreakerState>,
    pools: std::collections::HashMap<String, u64>,
    /// The in-process queue of the `queue` strategy, one slot per held request
    /// with the instant it was enqueued at.
    queue: std::collections::HashMap<String, Vec<Instant>>,
}

impl RateLimiterRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The counter key of one request: the `{resource_type}:{resource_id}`
    /// prefix of the resource whose `rate_limit` the effective limit came
    /// from, followed by the scope, the scope's identifier, and the effective
    /// window (§1.5).
    #[must_use]
    pub fn counter_key(
        resource_type: &str,
        resource_id: &str,
        scope: RateLimitScope,
        scope_id: &str,
        window: Option<Window>,
    ) -> String {
        format!(
            "{resource_type}:{resource_id}:{scope:?}:{scope_id}:{:?}",
            window_millis(window)
        )
    }

    /// The bucket held for a key, initializing an absent one at full capacity
    /// so a first burst is admitted up to `burst.capacity`.
    pub fn bucket(
        &mut self,
        key: &str,
        capacity: u64,
        sustained: &Sustained,
        now: Instant,
    ) -> &mut TokenBucket {
        self.buckets
            .entry(key.to_owned())
            .or_insert_with(|| TokenBucket::full(capacity, sustained, now))
    }

    /// The window held for a key.
    pub fn window(&mut self, key: &str) -> &mut SlidingWindow {
        self.windows.entry(key.to_owned()).or_default()
    }

    /// The breaker machine held for an upstream, initializing an absent one at
    /// `closed`, which is the posture of a target whose configuration was
    /// rewritten.
    pub fn breaker(&mut self, upstream_prefix: &str) -> &mut CircuitBreakerState {
        self.breakers
            .entry(upstream_prefix.to_owned())
            .or_default()
    }

    /// The budget pool held for a key.
    pub fn pool(&mut self, key: &str, total: u64) -> &mut u64 {
        self.pools.entry(key.to_owned()).or_insert(total)
    }

    /// The number of requests the queue for a key holds.
    #[must_use]
    pub fn queue_len(&self, key: &str) -> usize {
        self.queue.get(key).map_or(0, Vec::len)
    }

    /// Enqueues one request's slot for a key.
    ///
    /// Returns `false` — and takes no slot — when the queue for the key
    /// already holds its count bound, so the bound is a property of the
    /// strategy and not a condition the caller can wait out.
    pub fn enqueue(&mut self, key: &str, now: Instant) -> bool {
        let slots = self.queue.entry(key.to_owned()).or_default();
        if slots.len() >= QUEUE_CAPACITY {
            return false;
        }
        slots.push(now);
        true
    }

    /// Removes one request's slot, which a released, an expired, or a
    /// disconnected request leaves the queue through; its slot returns to the
    /// bound and it is charged nothing by the removal itself.
    pub fn dequeue(&mut self, key: &str) {
        if let Some(slots) = self.queue.get_mut(key) {
            slots.pop();
            if slots.is_empty() {
                self.queue.remove(key);
            }
        }
    }

    /// Drops the expired slots of a key's queue: the requests that outwaited
    /// the wait bound, which are answered 429 by their own wait and charged
    /// nothing.
    ///
    /// Returns the number of slots the queue dropped, which is a diagnostic
    /// value and no condition any caller branches on.
    pub fn dequeue_expired(&mut self, key: &str, now: Instant) -> usize {
        let Some(slots) = self.queue.get_mut(key) else {
            return 0;
        };
        let before = slots.len();
        slots.retain(|at| now.saturating_duration_since(*at) < QUEUE_WAIT);
        let dropped = before - slots.len();
        if slots.is_empty() {
            self.queue.remove(key);
        }
        dropped
    }

    /// Drops every entry whose key begins with the given prefix and returns
    /// the number of entries dropped, which is a diagnostic value and no
    /// condition any caller branches on.
    pub fn drop_prefix(&mut self, prefix: &str) -> usize {
        let mut dropped = 0;
        let before = self.buckets.len();
        self.buckets.retain(|key, _| !key.starts_with(prefix));
        dropped += before - self.buckets.len();
        let before = self.windows.len();
        self.windows.retain(|key, _| !key.starts_with(prefix));
        dropped += before - self.windows.len();
        let before = self.breakers.len();
        self.breakers.retain(|key, _| !key.starts_with(prefix));
        dropped += before - self.breakers.len();
        let before = self.pools.len();
        self.pools.retain(|key, _| !key.starts_with(prefix));
        dropped += before - self.pools.len();
        let before = self.queue.len();
        self.queue.retain(|key, _| !key.starts_with(prefix));
        dropped += before - self.queue.len();
        dropped
    }
}
