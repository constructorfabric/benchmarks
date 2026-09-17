//! Dual-rate bucket enforcement (algorithm
//! `cpt-cf-oagw-algo-rate-limiting-token-bucket`, ADR 0003).
//!
//! Two strategies backed by the same dual-rate parameters (`sustained.rate` +
//! `sustained.window`, `burst.capacity` defaulting to the sustained rate):
//!
//! - **Token bucket** — allows bursts up to the burst capacity and then enforces
//!   the sustained rate (steps `inst-rl-bucket-refill` /
//!   `inst-rl-bucket-consume`).  Tokens refill continuously at
//!   `rate / window_seconds` per second, capped at the capacity.
//! - **Sliding window** — a sliding-window counter bounded by the burst
//!   capacity so no boundary burst is possible.
//!
//! Both expose the same surface: `try_acquire(cost)` plus the header
//! projections `capacity()` / `remaining()` / `time_until_full()`
//! (`inst-rl-bucket-track`).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::domain::entity::config::{RateLimitAlgorithm, RateLimitConfig, RateLimitWindow};

/// Seconds represented by a [`RateLimitWindow`] (ADR 0003 field
/// `sustained.window`).
#[must_use]
pub const fn window_seconds(window: RateLimitWindow) -> u64 {
    match window {
        RateLimitWindow::Second => 1,
        RateLimitWindow::Minute => 60,
        RateLimitWindow::Hour => 3600,
        RateLimitWindow::Day => 86_400,
    }
}

/// Immutable dual-rate parameters derived from a [`RateLimitConfig`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BucketParams {
    /// Burst capacity (bucket size; defaults to the sustained rate).
    capacity: f64,
    /// Sustained replenishment rate in tokens per second.
    refill_rate: f64,
    /// The sustained window (sliding-window width).
    window: RateLimitWindow,
    /// The algorithm selected for this bucket.
    algorithm: RateLimitAlgorithm,
}

impl BucketParams {
    /// Derives the parameters from an effective [`RateLimitConfig`].
    #[must_use]
    pub fn from_config(config: &RateLimitConfig) -> Self {
        Self::from_dual_rate(
            config.sustained.rate,
            config.sustained.window,
            config.burst.as_ref().and_then(|b| b.capacity),
            config.algorithm,
        )
    }

    /// Derives the parameters from the ADR dual-rate fields.  A zero
    /// `sustained_rate` is clamped to 1 so the refill math never divides by
    /// zero; `burst_capacity` defaults to the sustained rate.
    #[must_use]
    pub fn from_dual_rate(
        sustained_rate: u64,
        window: RateLimitWindow,
        burst_capacity: Option<u64>,
        algorithm: RateLimitAlgorithm,
    ) -> Self {
        let rate = sustained_rate.max(1) as f64;
        Self {
            capacity: burst_capacity.unwrap_or(sustained_rate).max(1) as f64,
            refill_rate: rate / window_seconds(window) as f64,
            window,
            algorithm,
        }
    }

    /// The burst capacity as an integer (the `X-RateLimit-Limit` value).
    #[must_use]
    pub fn capacity(&self) -> u64 {
        self.capacity as u64
    }

    /// A collision-tolerant fingerprint so a bucket whose effective config
    /// changed is re-created instead of reused.
    #[must_use]
    pub fn fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.capacity.to_bits().hash(&mut hasher);
        self.refill_rate.to_bits().hash(&mut hasher);
        window_seconds(self.window).hash(&mut hasher);
        match self.algorithm {
            RateLimitAlgorithm::TokenBucket => 0_u8.hash(&mut hasher),
            RateLimitAlgorithm::SlidingWindow => 1_u8.hash(&mut hasher),
        }
        hasher.finish()
    }
}

/// The classic token bucket (ADR 0003 implementation notes).
#[derive(Debug, Clone)]
pub struct TokenBucket {
    tokens: f64,
    last_update: Instant,
    params: BucketParams,
}

impl TokenBucket {
    /// Creates a full bucket at the given instant.
    #[must_use]
    pub fn new(params: BucketParams, now: Instant) -> Self {
        Self {
            tokens: params.capacity,
            last_update: now,
            params,
        }
    }

    /// Refills tokens at the sustained rate up to the capacity (step
    /// `inst-rl-bucket-refill`).
    fn refill(&mut self, now: Instant) {
        if now <= self.last_update {
            return;
        }
        let elapsed = now.duration_since(self.last_update).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.params.refill_rate).min(self.params.capacity);
        self.last_update = now;
    }

    /// The token count after a hypothetical refill at `now` (non-mutating).
    #[must_use]
    fn peek(&self, now: Instant) -> f64 {
        if now <= self.last_update {
            return self.tokens;
        }
        let elapsed = now.duration_since(self.last_update).as_secs_f64();
        (self.tokens + elapsed * self.params.refill_rate).min(self.params.capacity)
    }

    /// Consumes `cost` tokens after refilling (step `inst-rl-bucket-consume`).
    ///
    /// Returns `false` when the refilled bucket holds fewer than `cost` tokens
    /// (step `inst-rl-bucket-empty`).
    pub fn try_acquire(&mut self, cost: u64, now: Instant) -> bool {
        self.refill(now);
        let cost = cost as f64;
        if self.tokens >= cost {
            self.tokens -= cost;
            true
        } else {
            false
        }
    }

    /// Remaining tokens after the decision (floored, so a reject reads 0).
    #[must_use]
    pub fn remaining(&self, now: Instant) -> u64 {
        self.peek(now).floor().max(0.0) as u64
    }

    /// Time until the bucket refills to capacity (advisory `reset` timing).
    #[must_use]
    pub fn time_until_full(&self, now: Instant) -> Duration {
        let deficit = (self.params.capacity - self.peek(now)).max(0.0);
        seconds_ceil(deficit / self.params.refill_rate)
    }

    /// Time until `cost` tokens are available again (`Retry-After`).
    #[must_use]
    pub fn time_until_available(&self, cost: u64, now: Instant) -> Duration {
        let need = (cost as f64 - self.peek(now)).max(0.0);
        seconds_ceil(need / self.params.refill_rate)
    }

    /// The burst capacity in tokens (`X-RateLimit-Limit`).
    #[must_use]
    pub fn capacity(&self) -> u64 {
        self.params.capacity()
    }
}

/// A sliding-window counter bounded by the burst capacity (ADR 0003 optional
/// `sliding_window` algorithm — smooths boundary bursts).
#[derive(Debug, Clone)]
pub struct SlidingWindowBucket {
    window: Duration,
    capacity: f64,
    /// `(request_timestamp, cost)` pairs, oldest first.
    requests: VecDeque<(Instant, f64)>,
}

impl SlidingWindowBucket {
    /// Creates an empty window at the given instant.
    #[must_use]
    pub fn new(params: BucketParams, _now: Instant) -> Self {
        Self {
            window: Duration::from_secs(window_seconds(params.window)),
            capacity: params.capacity,
            requests: VecDeque::new(),
        }
    }

    /// Drops requests that fell out of the window (implied by step
    /// `inst-rl-bucket-refill`).
    fn expire(&mut self, now: Instant) {
        while let Some(&(ts, _)) = self.requests.front() {
            if now.duration_since(ts) >= self.window {
                self.requests.pop_front();
            } else {
                break;
            }
        }
    }

    /// Current in-window cost.
    #[must_use]
    fn used(&self, now: Instant) -> f64 {
        // Callers always `expire` before this method on the mutating path; a
        // non-mutating peek is not needed because the counter never refills.
        let _ = now;
        self.requests.iter().map(|(_, c)| c).sum()
    }

    /// Consumes `cost` tokens within the current window (steps
    /// `inst-rl-bucket-consume`/`inst-rl-bucket-empty`).
    pub fn try_acquire(&mut self, cost: u64, now: Instant) -> bool {
        self.expire(now);
        let cost = cost as f64;
        if self.used(now) + cost <= self.capacity {
            self.requests.push_back((now, cost));
            true
        } else {
            false
        }
    }

    /// Remaining budget in the current window.
    #[must_use]
    pub fn remaining(&self, now: Instant) -> u64 {
        let mut used = 0.0;
        for &(ts, c) in &self.requests {
            if now.duration_since(ts) < self.window {
                used += c;
            }
        }
        (self.capacity - used).max(0.0) as u64
    }

    /// Time until the window empties (resets).
    #[must_use]
    pub fn time_until_full(&self, now: Instant) -> Duration {
        match self
            .requests
            .iter()
            .find(|(ts, _)| now.duration_since(*ts) < self.window)
        {
            Some(&(ts, _)) => {
                let age = now.duration_since(ts);
                self.window.saturating_sub(age)
            }
            None => Duration::ZERO,
        }
    }

    /// Time until `cost`-worth of budget frees by expiry of the oldest
    /// requests.
    #[must_use]
    pub fn time_until_available(&self, cost: u64, now: Instant) -> Duration {
        let need = (cost as f64 - self.remaining(now) as f64).max(0.0);
        if need <= 0.0 {
            return Duration::ZERO;
        }
        // Walk oldest-first until enough budget has expired.
        let mut freed = 0.0;
        let mut when = self.window;
        for &(ts, c) in &self.requests {
            let age = now.duration_since(ts);
            if age >= self.window {
                continue;
            }
            freed += c;
            let time_to_expire = self.window.saturating_sub(age);
            // This request at `ts` expires at `ts + window`, freeing `freed`
            // budget by then.
            if freed >= need {
                when = time_to_expire;
                break;
            }
        }
        // A full window is the upper bound (budget expires fastest when the
        // oldest requests carry the most cost, so the walk above is the
        // minimum wait for `need` worth of expirations).
        when
    }

    /// The burst capacity in tokens (`X-RateLimit-Limit`).
    #[must_use]
    pub fn capacity(&self) -> u64 {
        self.capacity as u64
    }
}

/// A dual-rate bucket dispatching on the configured algorithm.
#[derive(Debug, Clone)]
pub enum Bucket {
    Token(TokenBucket),
    Sliding(SlidingWindowBucket),
}

impl Bucket {
    /// Creates the configured bucket kind at the given instant.
    #[must_use]
    pub fn new(params: &BucketParams, now: Instant) -> Self {
        match params.algorithm {
            RateLimitAlgorithm::TokenBucket => Bucket::Token(TokenBucket::new(*params, now)),
            RateLimitAlgorithm::SlidingWindow => {
                Bucket::Sliding(SlidingWindowBucket::new(*params, now))
            }
        }
    }

    /// Attempts to consume `cost` tokens (step `inst-rl-bucket-consume`).
    pub fn try_acquire(&mut self, cost: u64, now: Instant) -> bool {
        match self {
            Bucket::Token(b) => b.try_acquire(cost, now),
            Bucket::Sliding(b) => b.try_acquire(cost, now),
        }
    }

    /// Remaining budget (step `inst-rl-bucket-track`).
    #[must_use]
    pub fn remaining(&self, now: Instant) -> u64 {
        match self {
            Bucket::Token(b) => b.remaining(now),
            Bucket::Sliding(b) => b.remaining(now),
        }
    }

    /// Time until the bucket resets to full.
    #[must_use]
    pub fn time_until_full(&self, now: Instant) -> Duration {
        match self {
            Bucket::Token(b) => b.time_until_full(now),
            Bucket::Sliding(b) => b.time_until_full(now),
        }
    }

    /// Time until `cost` tokens are available (`Retry-After`).
    #[must_use]
    pub fn time_until_available(&self, cost: u64, now: Instant) -> Duration {
        match self {
            Bucket::Token(b) => b.time_until_available(cost, now),
            Bucket::Sliding(b) => b.time_until_available(cost, now),
        }
    }

    /// The burst capacity (`X-RateLimit-Limit`).
    #[must_use]
    pub fn capacity(&self) -> u64 {
        match self {
            Bucket::Token(b) => b.capacity(),
            Bucket::Sliding(b) => b.capacity(),
        }
    }
}

/// Rounds a fractional number of seconds up to a whole [`Duration`], clamped
/// to at least one second for a positive input.
fn seconds_ceil(secs: f64) -> Duration {
    if secs <= 0.0 {
        return Duration::ZERO;
    }
    Duration::from_secs(secs.ceil().max(1.0) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    const SECOND: Duration = Duration::from_secs(1);

    fn params(rate: u64, window: RateLimitWindow, burst: Option<u64>) -> BucketParams {
        BucketParams::from_dual_rate(rate, window, burst, RateLimitAlgorithm::TokenBucket)
    }

    #[test]
    fn token_bucket_allows_burst_then_enforces_sustained_rate() {
        // sustained 2/sec, burst capacity 5.
        let p = params(2, RateLimitWindow::Second, Some(5));
        let t0 = Instant::now();
        let mut b = TokenBucket::new(p, t0);

        // Immediate burst consumes the full capacity (5 tokens).
        for _ in 0..5 {
            assert!(b.try_acquire(1, t0), "burst of 5 allowed");
        }
        assert!(!b.try_acquire(1, t0), "capacity exhausted");
        assert_eq!(b.remaining(t0), 0);

        // After 3 seconds the bucket refilled 2*3 = 6 → capped at 5, capped.
        let t3 = t0 + Duration::from_secs(3);
        assert_eq!(b.remaining(t3), 5);
        for _ in 0..5 {
            assert!(b.try_acquire(1, t3), "refilled burst allowed");
        }
        assert!(!b.try_acquire(1, t3));

        // Sustained rate: only the 300ms worth of refill is available.
        let t3_short = t3 + Duration::from_millis(500);
        assert!(b.try_acquire(1, t3_short), "0.5s at 2/s → 1 token");
        assert!(!b.try_acquire(1, t3_short), "no tokens left");
        assert_eq!(b.remaining(t3_short), 0);
    }

    #[test]
    fn burst_capacity_defaults_to_sustained_rate() {
        // No burst config → capacity = sustained rate (2).
        let p = params(2, RateLimitWindow::Second, None);
        let t0 = Instant::now();
        let mut b = TokenBucket::new(p, t0);
        assert_eq!(b.capacity(), 2);
        assert!(b.try_acquire(1, t0));
        assert!(b.try_acquire(1, t0));
        assert!(!b.try_acquire(1, t0));
    }

    #[test]
    fn reject_timing_is_ceil_of_refill_gap() {
        let p = params(1, RateLimitWindow::Second, Some(1));
        let t0 = Instant::now();
        let mut b = TokenBucket::new(p, t0);
        assert!(b.try_acquire(1, t0));
        // 0 tokens available; 1 token refills in 1s → Retry-After 1s.
        assert_eq!(b.time_until_available(1, t0), SECOND);
        // After 500ms still less than 1s away.
        assert_eq!(
            b.time_until_available(1, t0 + Duration::from_millis(500)),
            SECOND
        );
    }

    #[test]
    fn sliding_window_resets_without_boundary_burst() {
        let p = BucketParams::from_dual_rate(
            2,
            RateLimitWindow::Second,
            Some(5),
            RateLimitAlgorithm::SlidingWindow,
        );
        let t0 = Instant::now();
        let mut b = SlidingWindowBucket::new(p, t0);

        for _ in 0..5 {
            assert!(b.try_acquire(1, t0), "burst of 5 within window");
        }
        assert!(!b.try_acquire(1, t0), "window full");

        // Nothing expired yet 900ms later; requests are still counted.
        let t09 = t0 + Duration::from_millis(900);
        assert!(!b.try_acquire(1, t09), "still inside the 1s window");

        // Once the window fully slides (>=1s), capacity returns gradually as
        // each request expires.
        let t1 = t0 + SECOND;
        assert!(b.try_acquire(1, t1), "first request aged out at 1s");
        assert_eq!(b.remaining(t1), 4);
    }

    #[test]
    fn sliding_window_retry_after_estimates_earliest_expiry() {
        let p = BucketParams::from_dual_rate(
            1,
            RateLimitWindow::Second,
            Some(1),
            RateLimitAlgorithm::SlidingWindow,
        );
        let t0 = Instant::now();
        let mut b = SlidingWindowBucket::new(p, t0);
        assert!(b.try_acquire(1, t0));
        // Capacity freed when the sole request expires (1s from now).
        assert_eq!(b.time_until_available(1, t0), SECOND);
    }
}
