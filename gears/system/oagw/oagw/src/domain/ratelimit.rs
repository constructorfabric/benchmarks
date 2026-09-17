//! Token-bucket rate limiting (`ADR 0003`).

use std::time::{Duration, Instant};

use dashmap::DashMap;
use uuid::Uuid;

use crate::domain::model::RateLimitConfig;

/// How long a bucket may sit untouched before the sweep drops it.
///
/// A scope key is minted per upstream/route and per caller identity, so the
/// registry grows with the traffic it has seen; without an idle window it would
/// hold an entry for every key ever used for the life of the process.
pub const BUCKET_IDLE_TTL: Duration = Duration::from_mins(15);

/// Number of buckets at which a `check` sweeps the idle ones.
///
/// Sweeping is O(buckets), so it is amortized over traffic instead of running
/// on every request.
const SWEEP_THRESHOLD: usize = 4096;

/// Outcome of a rate-limit check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateDecision {
    /// Whether the request is admitted.
    pub allowed: bool,
    /// Configured sustained rate (requests per window), for `X-RateLimit-Limit`.
    pub limit: u64,
    /// Requests still available in the bucket, for `X-RateLimit-Remaining`.
    pub remaining: u64,
    /// Seconds until the bucket is full again, for `X-RateLimit-Reset`.
    pub reset_secs: u64,
    /// Seconds until the next token, for `Retry-After` on a rejection.
    pub retry_after_secs: u64,
}

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    updated: Instant,
}

/// A bucket quantity as a whole number of tokens or seconds.
///
/// `u64` has no `TryFrom<f64>`, and `num-traits` is not a dependency, so the
/// conversion is a cast — but a provably safe one. Every operand fed in here is
/// clamped non-negative first, and all of them derive from `u32`-bounded rates
/// and capacities divided by a window of at least one second, so the largest
/// possible value is `u32::MAX * 86_400`, far inside `u64`. The cast therefore
/// neither truncates a meaningful value nor flips a sign, and a `NaN` or an
/// overflow lands on the same `0` / `u64::MAX` the `as` cast produced before.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn whole_seconds(value: f64) -> u64 {
    f64::max(value, 0.0) as u64
}

impl Bucket {
    fn new(capacity: f64) -> Self {
        Self {
            tokens: capacity,
            updated: Instant::now(),
        }
    }

    fn refill(&mut self, capacity: f64, rate: f64) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.updated).as_secs_f64();
        self.updated = now;
        self.tokens = (self.tokens + elapsed * rate).min(capacity);
    }
}

/// Process-local bucket registry keyed by an opaque scope key.
#[derive(Debug)]
pub struct RateLimiter {
    buckets: DashMap<String, Bucket>,
    idle_ttl: Option<Duration>,
    sweep_threshold: usize,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::tuned(BUCKET_IDLE_TTL, SWEEP_THRESHOLD)
    }
}

impl RateLimiter {
    /// Create an empty registry with the documented idle window.
    #[must_use]
    pub fn new() -> Self {
        Self::tuned(BUCKET_IDLE_TTL, SWEEP_THRESHOLD)
    }

    /// Create a registry with an explicit idle window and sweep threshold, for
    /// a deployment (or a test) that needs the bounds visible on the surface.
    #[must_use]
    pub fn tuned(idle_ttl: Duration, sweep_threshold: usize) -> Self {
        Self {
            buckets: DashMap::new(),
            idle_ttl: Some(idle_ttl),
            sweep_threshold,
        }
    }

    /// Consume `cost` tokens from the bucket identified by `key`.
    #[must_use]
    pub fn check(&self, key: &str, config: &RateLimitConfig, cost: u64) -> RateDecision {
        let capacity = f64::from(u32::try_from(config.capacity()).unwrap_or(u32::MAX));
        let rate = config.tokens_per_second();
        let limit = config.limit();
        let cost = f64::from(u32::try_from(cost).unwrap_or(u32::MAX));

        self.sweep_if_crowded();

        let mut entry = self
            .buckets
            .entry(key.to_owned())
            .or_insert_with(|| Bucket::new(capacity));
        let bucket = entry.value_mut();
        bucket.refill(capacity, rate);

        let allowed = bucket.tokens >= cost && cost <= capacity;
        if allowed {
            bucket.tokens -= cost;
        }
        let remaining = f64::max(bucket.tokens, 0.0);
        let missing = f64::max(cost - bucket.tokens, 0.0);
        let retry_after = if rate <= 0.0 {
            config.sustained.window.secs().max(1)
        } else {
            whole_seconds((missing / rate).ceil().max(1.0))
        };
        let missing_full = f64::max(capacity - bucket.tokens, 0.0);
        let reset = if rate <= 0.0 {
            0
        } else {
            whole_seconds((missing_full / rate).ceil())
        };

        RateDecision {
            allowed,
            limit,
            remaining: whole_seconds(remaining),
            reset_secs: reset,
            retry_after_secs: retry_after,
        }
    }

    /// Number of live buckets.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buckets.len()
    }

    /// Whether no bucket is live.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }

    /// Drop every bucket whose last refill is older than the idle window.
    ///
    /// Called from `check` once the registry outgrows its threshold, so the
    /// memory a retired scope key holds is bounded without a background task.
    pub fn sweep(&self) {
        let Some(idle_ttl) = self.idle_ttl else {
            return;
        };
        // A window this long cannot overrun the platform's `Instant` range, but
        // if it ever did, no bucket is older than it and nothing is retired.
        let Some(cutoff) = Instant::now().checked_sub(idle_ttl) else {
            return;
        };
        self.buckets.retain(|_, bucket| bucket.updated > cutoff);
    }

    /// Sweep only when the registry has outgrown its threshold.
    fn sweep_if_crowded(&self) {
        if self.buckets.len() > self.sweep_threshold {
            self.sweep();
        }
    }

    /// Drop the buckets of `upstream` and of the routes it owned.
    ///
    /// Called when the upstream is deleted: its scope keys can never be
    /// requested again, so the buckets they hold are pure overhead.
    pub fn drop_upstream(&self, upstream: Uuid, routes: &[Uuid]) {
        let prefix = format!("upstream:{upstream}");
        self.buckets.retain(|key, _| !key.starts_with(&prefix));
        for route in routes {
            let prefix = format!("route:{route}");
            self.buckets.retain(|key, _| !key.starts_with(&prefix));
        }
    }

    /// Drop every bucket (used when an upstream is deleted).
    pub fn clear(&self) {
        self.buckets.clear();
    }
}

/// Scope key for a rate limit: the configured scope selects the identity
/// component (`ADR 0003` "Scoping").
#[must_use]
pub fn scope_key(prefix: &str, scope: crate::domain::model::RateScope, identity: &str) -> String {
    format!("{prefix}:{}:{identity}", scope_tag(scope))
}

/// Stable label of a rate-limit scope.
#[must_use]
fn scope_tag(scope: crate::domain::model::RateScope) -> &'static str {
    use crate::domain::model::RateScope;
    match scope {
        RateScope::Global => "global",
        RateScope::Tenant => "tenant",
        RateScope::User => "user",
        RateScope::Ip => "ip",
        RateScope::Route => "route",
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::time::Duration;

    use crate::domain::model::{
        Burst, RateAlgorithm, RateScope, RateStrategy, RateWindow, SustainedRate,
    };

    use super::*;

    fn config(sustained: u64, window: RateWindow, burst: Option<u64>) -> RateLimitConfig {
        RateLimitConfig {
            sharing: crate::domain::model::SharingMode::default(),
            algorithm: RateAlgorithm::TokenBucket,
            sustained: SustainedRate {
                rate: sustained,
                window,
            },
            burst: burst.map(|capacity| Burst { capacity }),
            scope: RateScope::Tenant,
            strategy: RateStrategy::Reject,
            cost: 1,
            response_headers: true,
        }
    }

    #[test]
    fn allows_under_the_limit() {
        let limiter = RateLimiter::new();
        let cfg = config(10, RateWindow::Minute, None);
        for _ in 0..10 {
            assert!(limiter.check("k", &cfg, 1).allowed);
        }
        assert!(!limiter.check("k", &cfg, 1).allowed);
    }

    #[test]
    fn buckets_are_independent_per_key() {
        let limiter = RateLimiter::new();
        let cfg = config(1, RateWindow::Minute, None);
        assert!(limiter.check("a", &cfg, 1).allowed);
        assert!(!limiter.check("a", &cfg, 1).allowed);
        assert!(limiter.check("b", &cfg, 1).allowed);
    }

    #[test]
    fn remaining_counts_down() {
        let limiter = RateLimiter::new();
        let cfg = config(5, RateWindow::Hour, None);
        let first = limiter.check("k", &cfg, 1);
        assert_eq!(first.limit, 5);
        assert_eq!(first.remaining, 4);
        let second = limiter.check("k", &cfg, 1);
        assert_eq!(second.remaining, 3);
    }

    #[test]
    fn burst_raises_capacity() {
        let limiter = RateLimiter::new();
        let cfg = config(1, RateWindow::Minute, Some(20));
        for _ in 0..20 {
            assert!(limiter.check("k", &cfg, 1).allowed);
        }
        assert!(!limiter.check("k", &cfg, 1).allowed);
    }

    #[test]
    fn cost_above_capacity_never_passes() {
        let limiter = RateLimiter::new();
        let mut cfg = config(10, RateWindow::Minute, None);
        cfg.cost = 100;
        assert!(!limiter.check("k", &cfg, 100).allowed);
    }

    #[test]
    fn scope_key_encodes_scope() {
        let key = scope_key("up", RateScope::Ip, "1.2.3.4");
        assert_eq!(key, "up:ip:1.2.3.4");
    }

    #[test]
    fn an_idle_bucket_is_swept_away() {
        // A zero threshold sweeps on every check, which is what makes the idle
        // window observable in milliseconds.
        let limiter = RateLimiter::tuned(Duration::from_millis(20), 0);
        let cfg = config(10, RateWindow::Second, None);
        assert!(limiter.check("retired", &cfg, 1).allowed);
        assert_eq!(limiter.len(), 1);

        std::thread::sleep(Duration::from_millis(40));
        assert!(limiter.check("current", &cfg, 1).allowed);
        assert_eq!(
            limiter.len(),
            1,
            "only the bucket that was just used remains"
        );
        assert!(limiter.check("current", &cfg, 1).allowed);
        assert_eq!(limiter.len(), 1);
    }

    #[test]
    fn a_fresh_bucket_survives_the_sweep() {
        let limiter = RateLimiter::tuned(Duration::from_mins(1), 0);
        let cfg = config(10, RateWindow::Second, None);
        for key in ["a", "b", "c"] {
            assert!(limiter.check(key, &cfg, 1).allowed, "{key}");
        }
        // Every bucket was refilled on the check that touched it, so nothing is
        // idle yet — the sweep must not clear the registry it just served.
        assert_eq!(limiter.len(), 3);
    }

    #[test]
    fn deleting_an_upstream_drops_its_buckets() {
        let limiter = RateLimiter::new();
        let cfg = config(10, RateWindow::Second, None);
        let upstream = uuid::Uuid::from_u128(0x1);
        let route = uuid::Uuid::from_u128(0x2);
        assert!(
            limiter
                .check(
                    &scope_key(&format!("upstream:{upstream}"), RateScope::Ip, "1.2.3.4"),
                    &cfg,
                    1
                )
                .allowed
        );
        assert!(
            limiter
                .check(
                    &scope_key(&format!("route:{route}"), RateScope::Ip, "1.2.3.4"),
                    &cfg,
                    1
                )
                .allowed
        );
        assert!(
            limiter
                .check(
                    &scope_key("upstream:other", RateScope::Ip, "1.2.3.4"),
                    &cfg,
                    1
                )
                .allowed
        );
        assert_eq!(limiter.len(), 3);

        limiter.drop_upstream(upstream, &[route]);
        assert_eq!(limiter.len(), 1, "only the unrelated bucket survives");
        assert!(
            limiter
                .check(
                    &scope_key("upstream:other", RateScope::Ip, "1.2.3.4"),
                    &cfg,
                    1
                )
                .allowed
        );
    }
}
