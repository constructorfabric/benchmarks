//! Token-bucket rate limiting (ADR-0003).
//!
//! Each policy gets one bucket per scope key. A bucket starts full, drains by the cost of
//! every admitted request and refills continuously at the sustained rate; the burst
//! capacity is the bucket size. The clock is injected so the refill maths is testable
//! without real time.

use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;

use crate::domain::upstream::{RateLimit, RateLimitAlgorithm, RateLimitScope, RateWindow};
use crate::security::SecurityContextHolder;

/// What one request cost and how long the caller must wait for the next token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    /// Tokens left in the bucket after this request.
    pub remaining: u64,
    /// Seconds until the bucket can admit another request of this cost.
    pub retry_after_secs: u64,
}

impl Decision {
    /// Whether the request was admitted.
    #[must_use]
    pub const fn allowed(&self) -> bool {
        self.retry_after_secs == 0
    }
}

/// The source of "now".
pub trait Clock: Send + Sync {
    /// The current instant.
    fn now(&self) -> std::time::Instant;
}

/// The real clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> std::time::Instant {
        std::time::Instant::now()
    }
}

/// A clock a test advances by hand.
#[derive(Debug)]
pub struct ManualClock {
    current: std::sync::Mutex<std::time::Instant>,
}

impl ManualClock {
    /// Starts the clock at its construction instant.
    #[must_use]
    pub fn new(start: std::time::Instant) -> Self {
        Self {
            current: std::sync::Mutex::new(start),
        }
    }

    /// Advances the clock.
    ///
    /// A poisoned mutex is recovered from: the instant inside is still the truth.
    pub fn advance(&self, by: Duration) {
        *self.current.lock().unwrap_or_else(std::sync::PoisonError::into_inner) += by;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> std::time::Instant {
        *self.current.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The sustained rate expressed in tokens per second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rate {
    /// Tokens replenished per second.
    pub per_second: u64,
    /// Bucket capacity.
    pub capacity: u64,
}

impl Rate {
    /// Converts a policy's sustained rate and burst into a per-second rate.
    ///
    /// A sub-second window is scaled up so the refill is not rounded to zero; a
    /// multi-second window is scaled down.
    #[must_use]
    pub fn from_policy(policy: &RateLimit) -> Self {
        let window_secs = policy.sustained.window.duration().as_secs().max(1);
        let per_second = policy.sustained.rate.div_ceil(window_secs);
        let capacity = policy
            .burst
            .as_ref()
            .map_or(policy.sustained.rate, |burst| burst.capacity)
            .max(per_second.max(1));
        Self {
            per_second: per_second.max(1),
            capacity,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    updated: std::time::Instant,
}

/// The registry of live buckets, keyed by the scope's identity.
pub struct RateLimiter {
    buckets: DashMap<String, Bucket>,
    clock: Arc<dyn Clock>,
}

impl RateLimiter {
    /// Builds a limiter on the real clock.
    #[must_use]
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            buckets: DashMap::new(),
            clock,
        }
    }

    /// The counter key for a policy scope.
    #[must_use]
    pub fn key(
        policy: &RateLimit,
        security: &SecurityContextHolder,
        route_id: &str,
        client_ip: &str,
    ) -> String {
        let scope = match policy.scope {
            RateLimitScope::Global => "global".to_owned(),
            RateLimitScope::Tenant => format!("tenant:{}", security.own_tenant()),
            RateLimitScope::User => format!("user:{}", security.security().subject_id()),
            RateLimitScope::Ip => format!("ip:{client_ip}"),
            RateLimitScope::Route => format!("route:{route_id}"),
        };
        format!("{scope}")
    }

    /// Spends `policy.cost` tokens from the bucket named by `key`.
    ///
    /// A policy with no cost consumes a single token.
    ///
    /// The refill maths is float arithmetic; the token counts it produces are counts,
    /// never a fraction of a request, so the conversions below only ever round a value
    /// that was integral already.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    #[must_use]
    pub fn check(&self, key: &str, policy: &RateLimit) -> Decision {
        let rate = Rate::from_policy(policy);
        let cost = policy.cost.max(1) as f64;
        let refill_per_sec = rate.per_second as f64;
        let capacity = rate.capacity as f64;
        let now = self.clock.now();

        let mut entry = self.buckets.entry(key.to_owned()).or_insert(Bucket {
            tokens: capacity,
            updated: now,
        });

        let elapsed = now.saturating_duration_since(entry.updated).as_secs_f64();
        let refilled = (entry.tokens + elapsed * refill_per_sec).min(capacity);
        if refilled >= cost {
            let remaining = (refilled - cost) as u64;
            entry.tokens = refilled - cost;
            entry.updated = now;
            Decision {
                remaining,
                retry_after_secs: 0,
            }
        } else {
            // Do not charge a rejected request; report when the bucket can pay.
            entry.tokens = refilled;
            entry.updated = now;
            let wait = (cost - refilled) / refill_per_sec;
            Decision {
                remaining: refilled as u64,
                retry_after_secs: (wait.ceil() as u64).max(1),
            }
        }
    }

    /// Reads the bucket's current fill without consuming anything.
    ///
    /// See [`Limiter::check`] for the conversion note.
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    #[must_use]
    pub fn peek(&self, key: &str, policy: &RateLimit) -> u64 {
        let rate = Rate::from_policy(policy);
        let Some(entry) = self.buckets.get(key) else {
            return rate.capacity;
        };
        let elapsed = self
            .clock
            .now()
            .saturating_duration_since(entry.updated)
            .as_secs_f64();
        ((entry.tokens + elapsed * rate.per_second as f64).min(rate.capacity as f64)) as u64
    }
}

/// The window length of a rate limit, in seconds, for `Retry-After` reporting.
#[must_use]
pub const fn window_secs(window: RateWindow) -> u64 {
    window.duration().as_secs()
}

/// Whether a policy uses the sliding-window algorithm (implemented as a token bucket in
/// this slice; the difference is not observable at the envelope level).
#[must_use]
pub const fn is_sliding_window(algorithm: RateLimitAlgorithm) -> bool {
    matches!(algorithm, RateLimitAlgorithm::SlidingWindow)
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod ratelimit_tests;
