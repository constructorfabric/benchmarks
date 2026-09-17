//! Token-bucket rate limiter (in-memory, per spec ADR-0003 MVP).
//!
//! The data plane enforces one bucket per effective rate limit; hierarchical
//! budgets are honored by merging ancestor limits with `min()` before
//! checking (stricter always wins), so a single bucket per scope is
//! sufficient for local enforcement. No cross-node coordination (Redis sync)
//! is implemented — that is explicitly future work in the ADR.

use std::sync::Mutex;
use std::time::Instant;

/// A single token bucket.
///
/// Refill is continuous: `refill_per_sec` tokens accumulate up to
/// `capacity`, and each request consumes `cost` tokens.
pub struct TokenBucket {
    tokens: f64,
    last_refill: Instant,
    /// Sustained refill rate (tokens per second).
    refill_per_sec: f64,
    /// Maximum accumulated tokens (burst capacity).
    capacity: f64,
    /// Tokens consumed per request.
    cost: f64,
}

impl TokenBucket {
    /// Build a bucket from a sustained rate per second, a burst capacity,
    /// and a per-request cost.
    pub fn new(refill_per_sec: f64, capacity: u64, cost: u64) -> Self {
        Self {
            tokens: capacity as f64,
            last_refill: Instant::now(),
            refill_per_sec: refill_per_sec.max(0.0),
            capacity: capacity.max(1) as f64,
            cost: cost.max(1) as f64,
        }
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.last_refill = now;
        self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
    }

    /// Try to consume `cost` tokens.
    fn try_consume(&mut self) -> LimitOutcome {
        self.refill();
        if self.tokens >= self.cost {
            self.tokens -= self.cost;
            return LimitOutcome::Allowed;
        }
        let needed = self.cost - self.tokens;
        let wait_secs = if self.refill_per_sec > 0.0 {
            (needed / self.refill_per_sec).ceil() as u64
        } else {
            u64::MAX
        };
        LimitOutcome::Rejected { wait_secs }
    }
}

/// Outcome of a rate-limit check.
pub enum LimitOutcome {
    /// The request may proceed; the bucket state was already mutated.
    Allowed,
    /// The request must be rejected. `wait_secs` is the seconds until the
    /// required tokens are available (for `Retry-After` / `X-RateLimit-Reset`).
    Rejected { wait_secs: u64 },
}

/// Snapshot of bucket state for the `X-RateLimit-*` response headers.
pub struct LimitSnapshot {
    /// Sustained limit count (requests per window criterion).
    pub limit: u64,
    /// Tokens remaining right now.
    pub remaining: u64,
    /// Seconds until at least `cost` tokens are available.
    pub reset_in: u64,
}

/// Concurrency-safe collection of token buckets keyed by scope.
#[derive(Default)]
pub struct RateLimiter {
    buckets: dashmap::DashMap<String, Mutex<TokenBucket>>,
}

impl RateLimiter {
    /// Check (and, if allowed, consume) one token-bucket for `key`,
    /// creating it with the given parameters on first use. Returns the
    /// decision plus the bucket snapshot for response headers.
    pub fn check(
        &self,
        key: &str,
        refill_per_sec: f64,
        capacity: u64,
        cost: u64,
    ) -> (LimitOutcome, LimitSnapshot) {
        let bucket = self
            .buckets
            .entry(key.to_string())
            .or_insert_with(|| Mutex::new(TokenBucket::new(refill_per_sec, capacity, cost)));
        let mut b = bucket.lock().unwrap();
        let outcome = b.try_consume();
        let snap = LimitSnapshot {
            limit: b.capacity.max(1.0) as u64,
            remaining: b.tokens.floor().max(0.0) as u64,
            reset_in: {
                if b.refill_per_sec <= 0.0 {
                    u64::MAX
                } else {
                    let need = b.cost - b.tokens.min(b.cost);
                    (need / b.refill_per_sec).ceil().max(0.0) as u64
                }
            },
        };
        (outcome, snap)
    }

    /// Clear all buckets (used by tests).
    pub fn clear(&self) {
        self.buckets.clear();
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_allows_within_capacity_then_rejects() {
        let rl = RateLimiter::default();
        let key = "t:1";
        let (o1, s1) = rl.check(key, 1.0, 2, 1);
        assert!(matches!(o1, LimitOutcome::Allowed));
        assert_eq!(s1.remaining, 1);
        let (o2, s2) = rl.check(key, 1.0, 2, 1);
        assert!(matches!(o2, LimitOutcome::Allowed));
        assert_eq!(s2.remaining, 0);
        let (o3, _s3) = rl.check(key, 1.0, 2, 1);
        assert!(matches!(o3, LimitOutcome::Rejected { wait_secs: _ }));
    }

    #[test]
    fn zero_rate_never_replenishes() {
        let rl = RateLimiter::default();
        let (o1, _) = rl.check("z", 0.0, 1, 1);
        assert!(matches!(o1, LimitOutcome::Allowed));
        let (o2, _) = rl.check("z", 0.0, 1, 1);
        assert!(matches!(o2, LimitOutcome::Rejected { .. }));
    }

    #[test]
    fn distinct_keys_have_independent_buckets() {
        let rl = RateLimiter::default();
        let (o1, _) = rl.check("a", 0.0, 1, 1);
        let (o2, _) = rl.check("b", 0.0, 1, 1);
        assert!(matches!(o1, LimitOutcome::Allowed));
        assert!(matches!(o2, LimitOutcome::Allowed));
    }
}
