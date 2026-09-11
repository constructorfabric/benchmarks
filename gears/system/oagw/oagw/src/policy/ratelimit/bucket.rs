//! Token-bucket accounting and replenishment
//! (`cpt-cf-oagw-algo-ratelimit-consume`) and the bucket's lifecycle
//! (`cpt-cf-oagw-state-ratelimit-bucket`).
//!
//! This module applies token-bucket mechanics uniformly regardless of the
//! configured `algorithm`; `cpt-cf-oagw-dod-ratelimit-token-bucket` records
//! that `algorithm: sliding_window` is accepted as a legal persisted value
//! without a distinct sliding-window implementation existing for this
//! round, so callers that select `sliding_window` still flow through this
//! same accounting.

use std::time::{Duration, Instant};

/// `cpt-cf-oagw-state-ratelimit-bucket`'s three states. `Created` is
/// transient (a key with no bucket yet); once seeded a bucket is always
/// either `HasTokens` or `Exhausted`, derived from its current token count
/// rather than tracked as separate stored state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BucketLifecycle {
    HasTokens,
    Exhausted,
}

/// A single token bucket's mutable state: the current token count and the
/// instant it was last replenished. This dual-rate representation --
/// `tokens` capped at `burst.capacity`, replenished at `sustained.rate` per
/// `sustained.window` in [`consume`] below -- is
/// `cpt-cf-oagw-dod-ratelimit-token-bucket`'s in-memory token-bucket
/// algorithm.
// @cpt-dod:cpt-cf-oagw-dod-ratelimit-token-bucket:p1
#[derive(Debug, Clone)]
pub(crate) struct TokenBucketState {
    tokens: f64,
    last_replenished_at: Instant,
    /// RF-003: fixed at seed time and never re-applied from a later call's
    /// config, so one route's/upstream's capacity can never transiently
    /// widen or narrow another config's bucket that happens to share this
    /// same counter key.
    capacity: u32,
}

impl TokenBucketState {
    /// `cpt-cf-oagw-state-ratelimit-bucket` transition Created -> HasTokens
    /// (`inst-state-ratelimit-bucket-01`): a bucket seen for the first time
    /// is seeded to `tokens = burst.capacity`, and that `capacity` is
    /// thereafter bucket-sticky (RF-003): [`consume`] never re-reads a
    /// per-call capacity again.
    // @cpt-begin:cpt-cf-oagw-state-ratelimit-bucket:p2:inst-state-ratelimit-bucket-01
    pub(crate) fn seeded(capacity: u32, now: Instant) -> Self {
        Self {
            tokens: f64::from(capacity),
            last_replenished_at: now,
            capacity,
        }
    }
    // @cpt-end:cpt-cf-oagw-state-ratelimit-bucket:p2:inst-state-ratelimit-bucket-01

    /// The remaining three `cpt-cf-oagw-state-ratelimit-bucket` transitions:
    /// HasTokens -> Exhausted when a consumption leaves `tokens < 1`
    /// (`inst-state-ratelimit-bucket-02`), Exhausted -> HasTokens once
    /// replenishment brings `tokens` back to `>= 1`
    /// (`inst-state-ratelimit-bucket-03`), and the HasTokens -> HasTokens
    /// self-loop when a consumption leaves `tokens >= 1`
    /// (`inst-state-ratelimit-bucket-04`). All three are this single
    /// derivation from the post-`consume` token count, exercised by
    /// `exhausting_the_bucket_denies_without_consuming_cost` and
    /// `replenishment_after_elapsed_time_returns_to_has_tokens` below.
    // @cpt-begin:cpt-cf-oagw-state-ratelimit-bucket:p2:inst-state-ratelimit-bucket-02
    // @cpt-begin:cpt-cf-oagw-state-ratelimit-bucket:p2:inst-state-ratelimit-bucket-03
    // @cpt-begin:cpt-cf-oagw-state-ratelimit-bucket:p2:inst-state-ratelimit-bucket-04
    fn lifecycle(&self) -> BucketLifecycle {
        if self.tokens >= 1.0 {
            BucketLifecycle::HasTokens
        } else {
            BucketLifecycle::Exhausted
        }
    }
    // @cpt-end:cpt-cf-oagw-state-ratelimit-bucket:p2:inst-state-ratelimit-bucket-04
    // @cpt-end:cpt-cf-oagw-state-ratelimit-bucket:p2:inst-state-ratelimit-bucket-03
    // @cpt-end:cpt-cf-oagw-state-ratelimit-bucket:p2:inst-state-ratelimit-bucket-02

    #[cfg(test)]
    pub(crate) fn tokens(&self) -> f64 {
        self.tokens
    }
}

/// `cpt-cf-oagw-algo-ratelimit-consume`'s ALLOW/DENY decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConsumeDecision {
    Allow,
    Deny,
}

/// `cpt-cf-oagw-algo-ratelimit-consume`'s output: the decision, the
/// remaining token count, the reset instant, and the bucket's resulting
/// lifecycle state.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ConsumeOutcome {
    pub decision: ConsumeDecision,
    pub remaining: u32,
    pub reset_at: Instant,
    /// Surfaced for `cpt-cf-oagw-state-ratelimit-bucket`'s own tests below
    /// and by any richer caller that wants to observe the transition
    /// directly; the current adapter (`crate::policy::ratelimit::evaluate_budget`)
    /// only branches on `decision`.
    #[allow(dead_code)]
    pub lifecycle: BucketLifecycle,
}

/// `cpt-cf-oagw-algo-ratelimit-consume` steps 2-9: replenish `state` for the
/// elapsed time since it was last touched, then attempt to subtract `cost`
/// tokens. `rate_per_second` is the effective `sustained.rate` already
/// normalized to a per-second basis by `cpt-cf-oagw-algo-ratelimit-effective-limit`.
/// The replenishment cap is `state`'s own bucket-sticky `capacity`
/// (RF-003), fixed at [`TokenBucketState::seeded`] time -- never the
/// calling config's own `burst.capacity`, so a second config that happens
/// to share this bucket's counter key can never transiently widen or
/// narrow it.
///
/// The caller (`crate::policy::ratelimit::engine`) is responsible for the
/// atomicity `inst-ratelimit-consume`'s closing note requires: this
/// function itself only performs the read-modify-write on the `&mut`
/// reference it is given, atomic per call, so the caller must hold that
/// bucket exclusively (e.g. behind a single `dashmap` shard lock) for the
/// duration of one `consume` call.
// @cpt-begin:cpt-cf-oagw-algo-ratelimit-consume:p1:inst-ratelimit-consume-03
// @cpt-begin:cpt-cf-oagw-algo-ratelimit-consume:p1:inst-ratelimit-consume-04
// @cpt-begin:cpt-cf-oagw-algo-ratelimit-consume:p1:inst-ratelimit-consume-05
pub(crate) fn consume(
    state: &mut TokenBucketState,
    rate_per_second: f64,
    cost: u32,
    now: Instant,
) -> ConsumeOutcome {
    let elapsed = now
        .checked_duration_since(state.last_replenished_at)
        .unwrap_or(Duration::ZERO);
    let replenished = state.tokens + elapsed.as_secs_f64() * rate_per_second;
    state.tokens = replenished.min(f64::from(state.capacity));
    state.last_replenished_at = now;
    // @cpt-end:cpt-cf-oagw-algo-ratelimit-consume:p1:inst-ratelimit-consume-05
    // @cpt-end:cpt-cf-oagw-algo-ratelimit-consume:p1:inst-ratelimit-consume-04
    // @cpt-end:cpt-cf-oagw-algo-ratelimit-consume:p1:inst-ratelimit-consume-03

    // @cpt-begin:cpt-cf-oagw-algo-ratelimit-consume:p1:inst-ratelimit-consume-06
    // @cpt-begin:cpt-cf-oagw-algo-ratelimit-consume:p1:inst-ratelimit-consume-07
    // @cpt-begin:cpt-cf-oagw-algo-ratelimit-consume:p1:inst-ratelimit-consume-08
    // @cpt-begin:cpt-cf-oagw-algo-ratelimit-consume:p1:inst-ratelimit-consume-09
    let decision = if state.tokens >= f64::from(cost) {
        state.tokens -= f64::from(cost);
        ConsumeDecision::Allow
    } else {
        ConsumeDecision::Deny
    };
    // @cpt-end:cpt-cf-oagw-algo-ratelimit-consume:p1:inst-ratelimit-consume-09
    // @cpt-end:cpt-cf-oagw-algo-ratelimit-consume:p1:inst-ratelimit-consume-08
    // @cpt-end:cpt-cf-oagw-algo-ratelimit-consume:p1:inst-ratelimit-consume-07
    // @cpt-end:cpt-cf-oagw-algo-ratelimit-consume:p1:inst-ratelimit-consume-06

    // @cpt-begin:cpt-cf-oagw-algo-ratelimit-consume:p1:inst-ratelimit-consume-10
    // @cpt-begin:cpt-cf-oagw-algo-ratelimit-consume:p1:inst-ratelimit-consume-11
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "tokens is clamped into [0, capacity] above, and capacity is a u32, so the floor \
                  always re-fits in u32"
    )]
    let remaining = state.tokens.max(0.0).floor() as u32;
    let reset_at = reset_instant(state.tokens, rate_per_second, now);
    // @cpt-end:cpt-cf-oagw-algo-ratelimit-consume:p1:inst-ratelimit-consume-11
    // @cpt-end:cpt-cf-oagw-algo-ratelimit-consume:p1:inst-ratelimit-consume-10

    ConsumeOutcome {
        decision,
        remaining,
        reset_at,
        lifecycle: state.lifecycle(),
    }
}

/// `inst-ratelimit-consume-11`: `now` if a token is already available,
/// else the earliest future instant at which one will be, computed from
/// the deficit and the replenishment rate. Never panics: a non-positive or
/// non-finite `rate_per_second` (never produced by a schema-valid
/// `sustained.rate >= 1`) falls back to a one-day horizon rather than
/// calling the panicking `Duration::from_secs_f64` with a bad input.
fn reset_instant(tokens: f64, rate_per_second: f64, now: Instant) -> Instant {
    if tokens >= 1.0 {
        return now;
    }
    let deficit = 1.0 - tokens;
    if rate_per_second > 0.0 && rate_per_second.is_finite() {
        now + Duration::from_secs_f64(deficit / rate_per_second)
    } else {
        now + Duration::from_secs(86_400)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::float_cmp, clippy::float_cmp_const)]
mod tests {
    use super::*;

    #[test]
    fn seeded_bucket_starts_at_full_capacity_and_has_tokens() {
        let now = Instant::now();
        let bucket = TokenBucketState::seeded(10, now);
        assert_eq!(bucket.tokens(), 10.0);
        assert_eq!(bucket.lifecycle(), BucketLifecycle::HasTokens);
    }

    #[test]
    fn consuming_within_capacity_allows_and_decrements() {
        let now = Instant::now();
        let mut bucket = TokenBucketState::seeded(5, now);
        let outcome = consume(&mut bucket, 1.0, 1, now);
        assert_eq!(outcome.decision, ConsumeDecision::Allow);
        assert_eq!(outcome.remaining, 4);
        assert_eq!(outcome.lifecycle, BucketLifecycle::HasTokens);
    }

    #[test]
    fn exhausting_the_bucket_denies_without_consuming_cost() {
        let now = Instant::now();
        let mut bucket = TokenBucketState::seeded(1, now);
        let first = consume(&mut bucket, 1.0, 1, now);
        assert_eq!(first.decision, ConsumeDecision::Allow);
        let second = consume(&mut bucket, 1.0, 1, now);
        assert_eq!(second.decision, ConsumeDecision::Deny);
        assert_eq!(second.remaining, 0);
        assert_eq!(second.lifecycle, BucketLifecycle::Exhausted);
        // Denial must not touch the token count further.
        assert_eq!(bucket.tokens(), 0.0);
    }

    #[test]
    fn replenishment_after_elapsed_time_returns_to_has_tokens() {
        let now = Instant::now();
        // Capacity 2 so that, after replenishment restores it to full, one
        // more consumption still leaves a token behind -- demonstrating the
        // Exhausted -> HasTokens transition itself, not merely a bare ALLOW.
        let mut bucket = TokenBucketState::seeded(2, now);
        assert_eq!(
            consume(&mut bucket, 1.0, 1, now).decision,
            ConsumeDecision::Allow
        );
        assert_eq!(
            consume(&mut bucket, 1.0, 1, now).decision,
            ConsumeDecision::Allow
        );
        let denied = consume(&mut bucket, 1.0, 1, now);
        assert_eq!(denied.decision, ConsumeDecision::Deny);
        assert_eq!(denied.lifecycle, BucketLifecycle::Exhausted);

        let later = now + Duration::from_secs(2);
        let allowed = consume(&mut bucket, 1.0, 1, later);
        assert_eq!(allowed.decision, ConsumeDecision::Allow);
        assert_eq!(allowed.lifecycle, BucketLifecycle::HasTokens);
    }

    #[test]
    fn reset_is_now_when_tokens_are_already_available() {
        let now = Instant::now();
        let mut bucket = TokenBucketState::seeded(5, now);
        let outcome = consume(&mut bucket, 1.0, 1, now);
        assert_eq!(outcome.reset_at, now);
    }

    #[test]
    fn reset_reflects_the_time_needed_for_one_more_token() {
        let now = Instant::now();
        let mut bucket = TokenBucketState::seeded(1, now);
        consume(&mut bucket, 2.0, 1, now);
        let outcome = consume(&mut bucket, 2.0, 1, now);
        assert_eq!(outcome.decision, ConsumeDecision::Deny);
        // Deficit is 1.0 token at a rate of 2/sec => 0.5s to next token.
        assert_eq!(outcome.reset_at, now + Duration::from_millis(500));
    }

    #[test]
    fn replenishment_never_exceeds_burst_capacity() {
        let now = Instant::now();
        let mut bucket = TokenBucketState::seeded(3, now);
        let far_future = now + Duration::from_secs(3600);
        let outcome = consume(&mut bucket, 10.0, 1, far_future);
        assert_eq!(outcome.remaining, 2);
    }

    /// RF-003: a bucket's capacity is fixed at seed time and is never
    /// re-widened or re-narrowed by a later call's own config -- `consume`
    /// no longer even takes a `capacity` parameter, only the seeded
    /// `TokenBucketState`'s own.
    #[test]
    fn capacity_stays_bucket_sticky_and_ignores_any_later_calls_intent() {
        let now = Instant::now();
        // Seed a small bucket, then replenish far enough into the future
        // that an *unbounded* bucket would have accrued far more tokens
        // than the seeded capacity -- proving the cap really is the
        // bucket's own sticky value, not something a later call could
        // widen.
        let mut bucket = TokenBucketState::seeded(2, now);
        let far_future = now + Duration::from_secs(1_000);
        let outcome = consume(&mut bucket, 1.0, 0, far_future);
        assert_eq!(
            outcome.remaining, 2,
            "replenishment must cap at 2, not drift"
        );
    }
}
