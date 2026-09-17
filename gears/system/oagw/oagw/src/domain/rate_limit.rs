//! Token-bucket rate limiter for the data plane.
//!
//! Implements the `cpt-cf-oagw-state-data-plane-rate-limiter-bucket` state
//! machine (`Ready` / `Exhausted`) and the fixed-window/token-bucket policy
//! the data-plane flow enforces with `429 + Retry-After`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use dashmap::DashMap;
use parking_lot::Mutex;

/// Fixed-point scale for token accounting. Refill rates are fractional
/// (tokens per second), so tokens are held scaled by this factor and one
/// admission consumes exactly `SCALE` units — keeping the math exact and
/// bursty tests deterministic.
const SCALE: u64 = 1_000_000;

/// One token bucket.
#[derive(Debug, Clone)]
pub struct TokenBucket {
    /// Burst capacity in whole tokens.
    capacity: u64,
    /// Held tokens as fixed-point (scaled by [`SCALE`]).
    tokens: u64,
    refill_per_sec: f64,
    last_refill: Instant,
}

impl TokenBucket {
    /// Creates a bucket with `capacity` and the given refill rate.
    #[must_use]
    pub fn new(capacity: u64, refill_per_sec: f64) -> Self {
        let capacity = capacity.max(1);
        Self {
            capacity,
            tokens: capacity.saturating_mul(SCALE),
            refill_per_sec: refill_per_sec.max(0.0),
            last_refill: Instant::now(),
        }
    }

    /// Reconciles the bucket against the current effective limits after a
    /// configuration change: resizes the capacity and swaps the refill rate,
    /// re-initializing the bucket under the re-applied limits so existing
    /// state never keeps stale settings that over- or under-admit.
    fn reconcile(&mut self, capacity: u64, refill_per_sec: f64) {
        let capacity = capacity.max(1);
        let refill_per_sec = refill_per_sec.max(0.0);
        if capacity != self.capacity || (refill_per_sec - self.refill_per_sec).abs() > f64::EPSILON
        {
            self.capacity = capacity;
            self.refill_per_sec = refill_per_sec;
            self.tokens = capacity.saturating_mul(SCALE);
            self.last_refill = Instant::now();
        }
    }

    /// Refills the bucket up to capacity based on elapsed time
    /// (`inst-refill`). Idempotent; called on every admission attempt.
    /// A zero refill rate means the bucket drains once and stays exhausted
    /// (`inst-remain-exhausted`).
    fn refill(&mut self) {
        // @cpt-begin:cpt-cf-oagw-state-data-plane-rate-limiter-bucket:ph-1:inst-refill
        if self.refill_per_sec <= 0.0 {
            return;
        }
        let elapsed = self.last_refill.elapsed();
        let added = (elapsed.as_secs_f64() * self.refill_per_sec * SCALE as f64).floor() as u64;
        self.tokens = (self.tokens + added).min(self.capacity.saturating_mul(SCALE));
        self.last_refill = Instant::now();
        // @cpt-end:cpt-cf-oagw-state-data-plane-rate-limiter-bucket:ph-1:inst-refill
    }

    /// Attempts to consume one token.
    ///
    /// - `Ready` with a token → consume and return `seconds_until_full`.
    /// - `Ready` without a token → `Exhausted` after refill attempt (`inst-exhaust`).
    /// - `Exhausted` stays exhausted while the bucket has not refilled (`inst-remain-exhausted`).
    #[must_use]
    pub fn try_consume(&mut self) -> BucketOutcome {
        self.refill();
        if self.tokens >= SCALE {
            // Computed before consuming: a full bucket admits with a 0s
            // refill hint; a partially drained bucket hints when it will
            // be full again.
            let wait_secs = self.seconds_until_full();
            // @cpt-begin:cpt-cf-oagw-state-data-plane-rate-limiter-bucket:ph-1:inst-consume
            self.tokens -= SCALE;
            // @cpt-end:cpt-cf-oagw-state-data-plane-rate-limiter-bucket:ph-1:inst-consume
            BucketOutcome::Admitted {
                retry_after_secs: wait_secs,
            }
        } else {
            // @cpt-begin:cpt-cf-oagw-state-data-plane-rate-limiter-bucket:ph-1:inst-exhaust
            // @cpt-begin:cpt-cf-oagw-state-data-plane-rate-limiter-bucket:ph-1:inst-remain-exhausted
            // (the refill attempt above did not replenish the bucket — it stays exhausted)
            // @cpt-end:cpt-cf-oagw-state-data-plane-rate-limiter-bucket:ph-1:inst-remain-exhausted
            BucketOutcome::Rejected {
                retry_after_secs: self.seconds_until_refill(),
            }
            // @cpt-end:cpt-cf-oagw-state-data-plane-rate-limiter-bucket:ph-1:inst-exhaust
        }
    }

    fn seconds_until_full(&self) -> u64 {
        if self.refill_per_sec <= 0.0 {
            return 0;
        }
        let missing = (self.capacity.saturating_mul(SCALE) - self.tokens) as f64
            / (self.refill_per_sec * SCALE as f64);
        missing.ceil() as u64
    }

    fn seconds_until_refill(&self) -> u64 {
        if self.refill_per_sec <= 0.0 {
            return 1;
        }
        (self.tokens as f64 / (self.refill_per_sec * SCALE as f64))
            .ceil()
            .max(1.0) as u64
    }
}

/// Outcome of a `try_consume` attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BucketOutcome {
    /// Request admitted; the retry hint is the seconds until the bucket is full.
    Admitted { retry_after_secs: u64 },
    /// Request rejected (`429`); the retry hint is the seconds until a refill.
    Rejected { retry_after_secs: u64 },
}

/// A registry of per-key token buckets (keyed by `alias|subject`).
pub struct RateLimiterRegistry {
    buckets: DashMap<Key, Arc<Mutex<TokenBucket>>>,
    enabled: AtomicU64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key(String);

impl RateLimiterRegistry {
    /// Creates a registry that consults `enabled` before enforcing.
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        Self {
            buckets: DashMap::new(),
            enabled: AtomicU64::new(u64::from(enabled)),
        }
    }

    /// Looks up (or lazily creates) the bucket for `key` with the given
    /// limits. Existing buckets are reconciled against the current limits so
    /// configuration changes resize capacity/refill instead of leaving stale
    /// state. When rate limiting is disabled globally, always admits.
    #[must_use]
    pub fn admit(&self, key: &str, capacity: u64, refill_per_sec: f64) -> BucketOutcome {
        if self.enabled.load(Ordering::Relaxed) == 0 {
            return BucketOutcome::Admitted {
                retry_after_secs: 0,
            };
        }
        // Existing bucket: reconcile limits before consuming.
        if let Some(existing) = self.buckets.get(&Key(key.to_owned())) {
            let bucket = existing.value().clone();
            bucket.lock().reconcile(capacity, refill_per_sec);
            return bucket.lock().try_consume();
        }
        let entry = self
            .buckets
            .entry(Key(key.to_owned()))
            .or_insert_with(|| Arc::new(Mutex::new(TokenBucket::new(capacity, refill_per_sec))));
        let bucket = entry.value().clone();
        bucket.lock().try_consume()
    }

    /// Removes every bucket whose key is bound to `route_alias` (keys are
    /// `alias|subject`), releasing rate-limit state when a route is deleted.
    pub fn prune(&self, route_alias: &str) {
        let prefix = format!("{route_alias}|");
        self.buckets.retain(|key, _| !key.0.starts_with(&prefix));
    }
}

impl Default for RateLimiterRegistry {
    fn default() -> Self {
        Self::new(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn bucket_admits_up_to_capacity_then_rejects() {
        let mut b = TokenBucket::new(2, 0.0); // no refill
        assert_eq!(
            b.try_consume(),
            BucketOutcome::Admitted {
                retry_after_secs: 0
            }
        );
        assert_eq!(
            b.try_consume(),
            BucketOutcome::Admitted {
                retry_after_secs: 0
            }
        );
        assert!(matches!(b.try_consume(), BucketOutcome::Rejected { .. }));
        // Still exhausted (inst-remain-exhausted).
        assert!(matches!(b.try_consume(), BucketOutcome::Rejected { .. }));
    }

    #[test]
    fn bucket_refills_over_time() {
        let mut b = TokenBucket::new(1, 100.0);
        assert_eq!(
            b.try_consume(),
            BucketOutcome::Admitted {
                retry_after_secs: 0
            }
        );
        assert!(matches!(b.try_consume(), BucketOutcome::Rejected { .. }));
        // Simulate one second of elapsed time by fast-forwarding.
        b.last_refill = Instant::now() - Duration::from_secs(1);
        assert_eq!(
            b.try_consume(),
            BucketOutcome::Admitted {
                retry_after_secs: 0
            }
        );
    }

    #[test]
    fn registry_disabled_admits_always() {
        let reg = RateLimiterRegistry::new(false);
        let out = reg.admit("k", 1, 0.0);
        assert!(matches!(out, BucketOutcome::Admitted { .. }));
    }

    #[test]
    fn registry_enabled_enforces() {
        let reg = RateLimiterRegistry::new(true);
        assert!(matches!(
            reg.admit("k", 1, 0.0),
            BucketOutcome::Admitted { .. }
        ));
        assert!(matches!(
            reg.admit("k", 1, 0.0),
            BucketOutcome::Rejected { .. }
        ));
        // Different key gets its own bucket.
        assert!(matches!(
            reg.admit("other", 1, 0.0),
            BucketOutcome::Admitted { .. }
        ));
    }

    /// A config change (capacity raise) must resize the existing bucket
    /// instead of leaving it stale (over-/under-admission fix).
    #[test]
    fn registry_reconciles_bucket_on_config_change() {
        let reg = RateLimiterRegistry::new(true);
        assert!(matches!(
            reg.admit("r|u", 1, 0.0),
            BucketOutcome::Admitted { .. }
        ));
        assert!(matches!(
            reg.admit("r|u", 1, 0.0),
            BucketOutcome::Rejected { .. }
        ));
        // Raise capacity on the same route key: the existing bucket must be
        // resized and admit again, not stay exhausted with stale settings.
        assert!(matches!(
            reg.admit("r|u", 3, 0.0),
            BucketOutcome::Admitted { .. }
        ));
        assert!(matches!(
            reg.admit("r|u", 3, 0.0),
            BucketOutcome::Admitted { .. }
        ));
    }

    /// Deleting a route must prune its per-subject buckets (no unbounded
    /// growth), while other routes' buckets survive.
    #[test]
    fn registry_prunes_buckets_per_route() {
        let reg = RateLimiterRegistry::new(true);
        for key in ["shop|u1", "shop|u2", "other|u1"] {
            let _ = reg.admit(key, 1, 0.0);
        }
        assert_eq!(reg.buckets.len(), 3);
        reg.prune("shop");
        assert_eq!(reg.buckets.len(), 1, "only non-pruned key remains");
        assert!(reg.buckets.contains_key(&Key("other|u1".to_owned())));
    }
}
