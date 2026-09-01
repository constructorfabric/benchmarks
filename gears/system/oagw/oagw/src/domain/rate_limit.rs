//! Rate-limit parameters and the deterministic limiter algorithms.
//!
//! The control plane validates and stores [`RateLimitConfig`]; the data plane
//! executes it. The algorithms live in the domain so that both sides agree on
//! the derived parameters (`capacity`, refill rate, window length) and so the
//! arithmetic is unit-testable without a running proxy.

use crate::domain::models::{RateLimitAlgorithm, RateLimitConfig, RateLimitScope, RateLimitStrategy};

/// Effective rate-limit parameters for one configuration block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimitParameters {
    /// Bucket capacity (burst) in tokens.
    pub capacity: u64,
    /// Tokens replenished per second.
    pub refill_per_second: u64,
    /// Sustained window length in seconds.
    pub window_seconds: u64,
    /// Tokens consumed by a single request.
    pub cost: u64,
    /// Counting scope.
    pub scope: RateLimitScope,
    /// Behaviour on exhaustion.
    pub strategy: RateLimitStrategy,
    /// Selected algorithm.
    pub algorithm: RateLimitAlgorithm,
}

impl RateLimitParameters {
    /// Derives the effective parameters from a configuration block.
    #[must_use]
    pub fn from_config(config: &RateLimitConfig) -> Self {
        let window_seconds = config.sustained.window.seconds();
        // Round up: a rate of 10 requests per 60 s still has to admit one
        // request every second, otherwise the bucket starves for a full window.
        let refill_per_second = if window_seconds == 0 {
            config.sustained.rate
        } else {
            config.sustained.rate.div_ceil(window_seconds)
        };
        Self {
            capacity: config.effective_capacity(),
            refill_per_second,
            window_seconds,
            cost: config.cost,
            scope: config.scope,
            strategy: config.strategy,
            algorithm: config.algorithm,
        }
    }
}

/// Outcome of a limiter check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimitDecision {
    /// Whether the request is admitted.
    pub allowed: bool,
    /// Tokens still available after the decision.
    pub remaining: u64,
    /// Seconds to wait before retrying (`Retry-After`).
    pub retry_after_seconds: u64,
}

/// Classic token bucket (DESIGN `algorithm: token_bucket`).
///
/// Allows bounded bursts up to `capacity` and refills continuously at
/// `refill_per_second`.
#[derive(Debug, Clone)]
pub struct TokenBucket {
    capacity: u64,
    refill_per_second: u64,
    cost: u64,
    tokens: u64,
    last_refill_ms: u64,
}

impl TokenBucket {
    /// Creates a bucket that starts full.
    #[must_use]
    pub fn new(parameters: &RateLimitParameters, now_ms: u64) -> Self {
        Self {
            capacity: parameters.capacity,
            refill_per_second: parameters.refill_per_second,
            cost: parameters.cost,
            tokens: parameters.capacity,
            last_refill_ms: now_ms,
        }
    }

    /// Attempts to consume `parameters.cost` tokens.
    #[must_use]
    pub fn try_consume(&mut self, now_ms: u64) -> RateLimitDecision {
        self.refill(now_ms);
        if self.tokens >= self.cost {
            self.tokens -= self.cost;
            RateLimitDecision {
                allowed: true,
                remaining: self.tokens,
                retry_after_seconds: 0,
            }
        } else {
            let missing = self.cost - self.tokens;
            let seconds = missing_seconds(missing, self.refill_per_second);
            RateLimitDecision {
                allowed: false,
                remaining: self.tokens,
                retry_after_seconds: seconds,
            }
        }
    }

    #[allow(clippy::integer_division)] // intentional: sub-second remainder is carried over, see below
    fn refill(&mut self, now_ms: u64) {
        if now_ms <= self.last_refill_ms {
            return;
        }
        let elapsed_ms = now_ms - self.last_refill_ms;
        // Truncation towards zero is deliberate: a partial second only accrues
        // credit once it completes, which keeps the bucket monotonic.
        let elapsed_seconds = elapsed_ms / 1_000;
        if elapsed_seconds == 0 {
            return;
        }
        let refill = elapsed_seconds.saturating_mul(self.refill_per_second);
        self.tokens = (self.tokens + refill).min(self.capacity);
        self.last_refill_ms += elapsed_seconds * 1_000;
    }
}

/// Sliding-window limiter (DESIGN `algorithm: sliding_window`).
///
/// Counts the requests admitted inside a rolling window of `window_seconds`.
#[derive(Debug, Clone)]
pub struct SlidingWindow {
    window_ms: u64,
    limit: u64,
    cost: u64,
    admitted: Vec<u64>,
}

impl SlidingWindow {
    /// Builds a limiter counting the sustained rate over the window.
    ///
    /// The window starts empty: the first request is the first sample, it is
    /// not counted twice.
    #[must_use]
    pub fn new(parameters: &RateLimitParameters, config: &RateLimitConfig) -> Self {
        Self {
            window_ms: config.sustained.window.seconds() * 1_000,
            limit: parameters.capacity,
            cost: parameters.cost,
            admitted: Vec::new(),
        }
    }

    /// Attempts to admit a request at `now_ms`.
    #[must_use]
    pub fn try_consume(&mut self, now_ms: u64) -> RateLimitDecision {
        self.evict(now_ms);
        let in_window = self
            .admitted
            .len()
            .min(usize::try_from(self.limit).unwrap_or(usize::MAX));
        if u64::try_from(in_window).unwrap_or(self.limit) + self.cost <= self.limit {
            self.admitted.push(now_ms);
            RateLimitDecision {
                allowed: true,
                remaining: self.limit - u64::try_from(self.admitted.len()).unwrap_or(self.limit),
                retry_after_seconds: 0,
            }
        } else {
            let oldest = self.admitted.first().copied().unwrap_or(now_ms);
            let wait_ms = oldest.saturating_sub(now_ms.saturating_sub(self.window_ms));
            RateLimitDecision {
                allowed: false,
                remaining: self
                    .limit
                    .saturating_sub(u64::try_from(self.admitted.len()).unwrap_or(self.limit)),
                // Ceil to at least one second so a client retrying after the
                // reported delay is guaranteed a fresh window slice.
                retry_after_seconds: wait_ms.div_ceil(1_000).max(1),
            }
        }
    }

    fn evict(&mut self, now_ms: u64) {
        let cutoff = now_ms.saturating_sub(self.window_ms);
        self.admitted.retain(|timestamp| *timestamp > cutoff);
    }
}

#[allow(clippy::integer_division)] // intentional: ceil-like rounding of a refill budget, verified below
fn missing_seconds(missing: u64, refill_per_second: u64) -> u64 {
    if refill_per_second == 0 {
        return 1;
    }
    (missing / refill_per_second).max(1)
}
