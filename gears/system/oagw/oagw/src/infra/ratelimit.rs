//! In-memory token-bucket rate limiting (ADR-0003).
//!
//! Dual-rate configuration: a sustained refill rate plus a burst capacity.
//! Buckets are owned by the data plane and keyed by
//! `scope:tenant:upstream:route` so per-route and hierarchical limits stay
//! independent. Distributed synchronization (Redis) is future work; the MVP
//! enforces locally per ADR-0003 §"Distribution".

use std::sync::Mutex;
use std::time::Instant;

use dashmap::DashMap;

/// Result of a rate-limit check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitDecision {
    /// True when the request is admitted.
    pub allowed: bool,
    /// Seconds until a rejected request should be retried (`Retry-After`).
    pub retry_after_secs: u64,
    /// Bucket capacity (for `X-RateLimit-Limit`).
    pub limit: u64,
    /// Tokens remaining after admission (for `X-RateLimit-Remaining`).
    pub remaining: u64,
    /// Seconds until the bucket refills to capacity (for `X-RateLimit-Reset`).
    pub reset_secs: u64,
}

struct Bucket {
    capacity: f64,
    refill_per_sec: f64,
    tokens: f64,
    last_refill: Instant,
}

impl Bucket {
    fn new(capacity: f64, refill_per_sec: f64) -> Self {
        Self {
            capacity,
            refill_per_sec,
            tokens: capacity,
            last_refill: Instant::now(),
        }
    }

    /// Refill based on elapsed time, then try to consume `cost` tokens.
    fn consume(&mut self, cost: f64) -> (bool, u64) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.last_refill = now;
        self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);

        if self.tokens >= cost {
            self.tokens -= cost;
            (true, self.retry_after_secs())
        } else {
            let wait: f64 = (cost - self.tokens) / self.refill_per_sec.max(1e-9);
            (false, wait.ceil().max(1.0) as u64)
        }
    }

    fn remaining(&self) -> f64 {
        self.tokens
    }

    fn retry_after_secs(&self) -> u64 {
        let to_full = (self.capacity - self.tokens) / self.refill_per_sec.max(1e-9);
        to_full.max(1.0) as u64
    }
}

/// Parse the RFC-style window string into seconds.
fn window_seconds(window: &str) -> u64 {
    match window.to_ascii_lowercase().as_str() {
        "minute" => 60,
        "hour" => 3600,
        "day" => 86400,
        _ => 1, // second (and unknown → second)
    }
}

/// Parse the algorithm name.
fn window_fraction(window: &str) -> u64 {
    window_seconds(window)
}

/// Token-bucket rate limiter registry.
#[derive(Default)]
pub struct RateLimiter {
    buckets: DashMap<String, Mutex<Bucket>>,
}

impl RateLimiter {
    /// Create a new, empty limiter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply a rate-limit decision for `key` with the given dual-rate
    /// configuration. `cost` is the number of tokens consumed per request.
    ///
    /// Returns the decision; when `strategy == "degrade"` an exhausted
    /// bucket still admits the request (records `allowed = true` with
    /// `remaining = 0` so callers can observe the exhaustion).
    pub fn check(
        &self,
        key: &str,
        sustained_rate: u32,
        window: &str,
        burst_capacity: u32,
        cost: u32,
        strategy: &str,
    ) -> RateLimitDecision {
        let window_secs = window_fraction(window).max(1);
        let refill_per_sec = (sustained_rate as f64) / (window_secs as f64);
        let capacity = burst_capacity.max(1) as f64;

        let bucket = self
            .buckets
            .entry(key.to_owned())
            .or_insert_with(|| Mutex::new(Bucket::new(capacity, refill_per_sec)));

        let mut guard = bucket
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (admitted, retry_after) = guard.consume(cost as f64);
        let remaining = guard.remaining().floor().max(0.0) as u64;
        let reset_secs = guard.retry_after_secs();

        let degrade = strategy.eq_ignore_ascii_case("degrade");
        RateLimitDecision {
            allowed: admitted || degrade,
            retry_after_secs: if admitted { reset_secs } else { retry_after },
            limit: capacity as u64,
            remaining,
            reset_secs,
        }
    }

    /// Prune buckets no longer referenced by any live configuration.
    pub fn retain(&self, live_keys: &std::collections::HashSet<String>) {
        self.buckets.retain(|key, _| live_keys.contains(key));
    }

    /// Number of live buckets (test/observability hook).
    #[must_use]
    pub fn len(&self) -> usize {
        self.buckets.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }
}

/// Effective rate-limit merge: stricter always wins
/// `effective = min(own, parent_effective)` (ADR-0003 §3).
///
/// Returns `None` when no rate limit applies.
#[must_use]
pub fn effective_rate_limit<'a>(
    chain: impl IntoIterator<Item = &'a crate::domain::model::RateLimitConfig>,
) -> Option<crate::domain::model::RateLimitConfig> {
    let mut merged: Option<crate::domain::model::RateLimitConfig> = None;
    let mut merged_pps: Option<f64> = None;

    for cfg in chain {
        let Some(sustained) = &cfg.sustained else {
            continue;
        };
        let pps = (sustained.rate as f64) / (window_seconds(&sustained.window).max(1) as f64);
        match &mut merged {
            None => {
                merged = Some(cfg.clone());
                merged_pps = Some(pps);
            }
            Some(current) => {
                let current_rate = current
                    .sustained
                    .as_ref()
                    .map(|s| (s.rate as f64) / (window_seconds(&s.window).max(1) as f64))
                    .unwrap_or(f64::MAX);
                if pps < current_rate {
                    *current = cfg.clone();
                    merged_pps = Some(pps);
                }
            }
        }
    }
    let _ = merged_pps;
    merged
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::model::{BurstConfig, RateLimitConfig, SustainedRate};

    #[test]
    fn token_bucket_allows_burst_then_rejects() {
        let limiter = RateLimiter::new();
        let key = "tenant:upstream:route:scope";
        // capacity 3, refill 1/s (window second)
        let d1 = limiter.check(key, 1, "second", 3, 1, "reject");
        assert!(d1.allowed);
        let d2 = limiter.check(key, 1, "second", 3, 1, "reject");
        assert!(d2.allowed);
        let d3 = limiter.check(key, 1, "second", 3, 1, "reject");
        assert!(d3.allowed);
        // Fourth exceeds burst capacity.
        let d4 = limiter.check(key, 1, "second", 3, 1, "reject");
        assert!(!d4.allowed);
        assert!(d4.retry_after_secs >= 1);
    }

    #[test]
    fn cost_scales_consumption() {
        let limiter = RateLimiter::new();
        let key = "t:u:r:s";
        let d1 = limiter.check(key, 1, "second", 5, 3, "reject");
        assert!(d1.allowed);
        // 5 - 3 = 2 remaining < 3 cost → reject
        let d2 = limiter.check(key, 1, "second", 5, 3, "reject");
        assert!(!d2.allowed);
    }

    #[test]
    fn degrade_admits_when_exhausted() {
        let limiter = RateLimiter::new();
        let key = "t:u:r:s";
        assert!(limiter.check(key, 1, "second", 1, 1, "reject").allowed);
        let d = limiter.check(key, 1, "second", 1, 1, "degrade");
        assert!(d.allowed, "degrade strategy must admit");
        assert_eq!(d.remaining, 0);
    }

    #[test]
    fn effective_rate_takes_strictest() {
        let per_hour = RateLimitConfig {
            sustained: Some(SustainedRate {
                rate: 10,
                window: "hour".to_owned(),
            }),
            burst: Some(BurstConfig { capacity: 1 }),
            ..Default::default()
        };
        let per_second = RateLimitConfig {
            sustained: Some(SustainedRate {
                rate: 5,
                window: "second".to_owned(),
            }),
            burst: Some(BurstConfig { capacity: 5 }),
            ..Default::default()
        };
        let effective = effective_rate_limit([&per_hour, &per_second]).unwrap();
        // Lowest requests-per-second wins: 10/hour ≈ 0.0028/s beats 5/s.
        let sustained = effective.sustained.unwrap();
        assert_eq!(sustained.rate, 10);
        assert_eq!(sustained.window, "hour");
    }

    #[test]
    fn no_rate_limit_yields_none() {
        assert!(effective_rate_limit([]).is_none());
    }

    #[test]
    fn window_fraction_second_works() {
        assert_eq!(window_seconds("second"), 1);
        assert_eq!(window_seconds("minute"), 60);
        assert_eq!(window_seconds("hour"), 3600);
        assert_eq!(window_seconds("day"), 86400);
    }

    #[test]
    fn buckets_isolated_by_key() {
        let limiter = RateLimiter::new();
        // Capacity-1 token buckets: each key admits once before refill.
        assert!(limiter.check("a", 1, "second", 1, 1, "reject").allowed);
        assert!(!limiter.check("a", 1, "second", 1, 1, "reject").allowed);
        // "b" is a distinct bucket → fresh admission.
        assert!(limiter.check("b", 1, "second", 1, 1, "reject").allowed);
        assert!(!limiter.check("b", 1, "second", 1, 1, "reject").allowed);
        // Depleting "b" again must not replenish "a".
        assert!(!limiter.check("a", 1, "second", 1, 1, "reject").allowed);
    }

    #[test]
    fn rate_limit_headers_reflect_bucket() {
        let limiter = RateLimiter::new();
        let key = "t:u:r:s";
        let d = limiter.check(key, 2, "second", 4, 1, "reject");
        assert_eq!(d.limit, 4);
        assert!(d.remaining <= 3);
        assert!(d.reset_secs >= 1);
    }

    #[test]
    fn retains_only_live() {
        let limiter = RateLimiter::new();
        limiter.check("dead", 1, "second", 1, 1, "reject");
        limiter.check("live", 1, "second", 1, 1, "reject");
        assert_eq!(limiter.len(), 2);
        let mut live = std::collections::HashSet::new();
        live.insert("live".to_owned());
        limiter.retain(&live);
        assert_eq!(limiter.len(), 1);
    }

    #[test]
    fn unknown_strategy_defaults_to_reject() {
        let limiter = RateLimiter::new();
        let key = "t:u:r:s";
        limiter.check(key, 1, "second", 1, 1, "reject");
        let d = limiter.check(key, 1, "second", 1, 1, "bogus");
        assert!(!d.allowed);
    }
}
