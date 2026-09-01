// Created: 2026-08-29 by Constructor Tech
//! Token bucket rate limiting (ADR-0003).
//!
//! `refill_rate = sustained.rate / window_seconds`, and the default bucket
//! capacity is `sustained.rate` when `burst.capacity` is absent.

use std::time::Instant;

/// Token bucket with lazy refill.
#[derive(Debug, Clone)]
pub struct TokenBucket {
    tokens: f64,
    last_update: Instant,
    capacity: f64,
    refill_rate: f64,
}

impl TokenBucket {
    /// Create a bucket that starts full.
    #[must_use]
    pub fn new(capacity: f64, refill_rate: f64) -> Self {
        Self {
            tokens: capacity,
            last_update: Instant::now(),
            capacity,
            refill_rate,
        }
    }

    /// Create a bucket pre-filled to `initial_tokens`.
    #[must_use]
    pub fn with_tokens(capacity: f64, refill_rate: f64, initial_tokens: f64) -> Self {
        let tokens = initial_tokens.clamp(0.0, capacity);
        Self {
            tokens,
            last_update: Instant::now(),
            capacity,
            refill_rate,
        }
    }

    /// Refill lazily based on the elapsed time.
    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_update).as_secs_f64();
        self.last_update = now;
        if self.refill_rate > 0.0 {
            self.tokens = (self.tokens + elapsed * self.refill_rate).min(self.capacity);
        }
    }

    /// Try to consume `cost` tokens.
    #[must_use]
    pub fn try_acquire(&mut self, cost: f64) -> bool {
        self.refill();
        if self.tokens + f64::EPSILON >= cost {
            self.tokens -= cost;
            true
        } else {
            false
        }
    }

    /// Seconds until the bucket holds at least `cost` tokens (for `Retry-After`).
    #[must_use]
    pub fn seconds_until_tokens(&self, cost: f64) -> u64 {
        let deficit = cost - self.current_tokens();
        if deficit <= 0.0 {
            return 0;
        }
        if self.refill_rate <= 0.0 {
            return u64::MAX;
        }
        ceil_to_u64(deficit / self.refill_rate)
    }

    /// Tokens currently available (without refilling).
    #[must_use]
    pub fn current_tokens(&self) -> f64 {
        let elapsed = Instant::now()
            .duration_since(self.last_update)
            .as_secs_f64();
        if self.refill_rate <= 0.0 {
            self.tokens
        } else {
            (self.tokens + elapsed * self.refill_rate).min(self.capacity)
        }
    }

    /// Unix epoch seconds at which the bucket is full again.
    #[must_use]
    pub fn epoch_seconds_until_full(&self) -> u64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        let deficit = self.capacity - self.current_tokens();
        if deficit <= 0.0 || self.refill_rate <= 0.0 {
            return now;
        }
        now + ceil_to_u64(deficit / self.refill_rate)
    }

    /// Bucket capacity.
    #[must_use]
    pub fn capacity(&self) -> f64 {
        self.capacity
    }

    /// Refill rate in tokens per second.
    #[must_use]
    pub fn refill_rate(&self) -> f64 {
        self.refill_rate
    }
}

/// Compute the refill rate in tokens per second.
#[must_use]
pub fn refill_rate(sustained_rate: u32, window_seconds: u64) -> f64 {
    if window_seconds == 0 {
        return 0.0;
    }
    f64::from(sustained_rate) / f64::from(u32::try_from(window_seconds).unwrap_or(u32::MAX))
}

/// Round a non-negative duration in seconds up to whole seconds.
fn ceil_to_u64(seconds: f64) -> u64 {
    let seconds = seconds.ceil();
    if seconds <= 1.0 {
        1
    } else if seconds >= f64::from(u32::MAX) {
        u64::from(u32::MAX)
    } else {
        seconds as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refills_over_time() {
        // 50 tokens/second: 30 ms restores at least one token.
        let mut bucket = TokenBucket::new(1.0, 50.0);
        assert!(bucket.try_acquire(1.0));
        assert!(!bucket.try_acquire(1.0));
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert!(bucket.try_acquire(1.0));
    }

    #[test]
    fn respects_capacity() {
        let mut bucket = TokenBucket::new(2.0, 100.0);
        // A single request cannot consume more than the capacity allows.
        assert!(bucket.try_acquire(2.0));
        assert!(!bucket.try_acquire(2.0));
    }

    #[test]
    fn retry_after_is_positive() {
        let mut bucket = TokenBucket::new(1.0, 1.0);
        assert!(bucket.try_acquire(1.0));
        assert!(bucket.seconds_until_tokens(1.0) >= 1);
    }

    #[test]
    fn window_seconds_mapping() {
        assert_eq!(refill_rate(10, 1), 10.0);
        assert!((refill_rate(600, 60) - 10.0).abs() < 1e-9);
        assert!((refill_rate(3_600, 3_600) - 1.0).abs() < 1e-9);
    }
}
