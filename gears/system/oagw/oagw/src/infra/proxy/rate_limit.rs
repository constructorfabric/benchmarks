//! Token buckets, one per `(resource, scope)` pair (ADR 0003).
//!
//! The registry holds one bucket per key over a [`dashmap`], refilling lazily
//! from an injectable clock so tests can advance time deterministically.

use async_trait::async_trait;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;

use crate::domain::model::RateLimit;
use crate::domain::services::data_plane::{RateLimitOutcome, RateLimiter};

/// Source of the instant a bucket is refilled against.
pub trait Clock: Send + Sync {
    /// The current instant.
    fn now(&self) -> Instant;
}

/// Wall-clock clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// A single bucket: tokens left and when the refill started.
#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

/// The token-bucket registry.
pub struct TokenBucketRegistry {
    buckets: DashMap<String, Bucket>,
    clock: Arc<dyn Clock>,
}

impl TokenBucketRegistry {
    /// A registry using the wall clock.
    #[must_use]
    pub fn new() -> Self {
        Self::with_clock(Arc::new(SystemClock))
    }

    /// A registry advancing against `clock`, for deterministic tests.
    #[must_use]
    pub fn with_clock(clock: Arc<dyn Clock>) -> Self {
        Self {
            buckets: DashMap::new(),
            clock,
        }
    }

    /// Number of buckets currently live.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buckets.len()
    }

    /// Whether the registry holds no bucket.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }

    /// Tokens left in the bucket for `key`, refilling it first without
    /// consuming anything.
    #[must_use]
    pub fn peek(&self, key: &str, limit: &RateLimit) -> f64 {
        let capacity = f64::from(limit.effective_capacity());
        let mut entry = self.buckets.entry(key.to_owned()).or_insert(Bucket {
            tokens: capacity,
            last_refill: self.clock.now(),
        });
        Self::refill(&mut entry, limit, self.clock.now(), capacity);
        entry.tokens
    }

    /// Drop every bucket; used by tests and by a future reconfiguration hook.
    pub fn clear(&self) {
        self.buckets.clear();
    }

    fn refill(entry: &mut Bucket, limit: &RateLimit, now: Instant, capacity: f64) {
        let elapsed = now.saturating_duration_since(entry.last_refill);
        if elapsed > Duration::ZERO {
            entry.tokens =
                (entry.tokens + limit.refill_per_sec() * elapsed.as_secs_f64()).min(capacity);
            entry.last_refill = now;
        }
    }
}

impl Default for TokenBucketRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RateLimiter for TokenBucketRegistry {
    async fn acquire(&self, key: &str, limit: &RateLimit, cost: u32) -> RateLimitOutcome {
        self.acquire_impl(key, limit, cost)
    }
}

impl TokenBucketRegistry {
    fn acquire_impl(&self, key: &str, limit: &RateLimit, cost: u32) -> RateLimitOutcome {
        let capacity = limit.effective_capacity();
        let capacity_f64 = f64::from(capacity);
        let cost = f64::from(cost.max(1));
        let now = self.clock.now();
        let mut entry = self.buckets.entry(key.to_owned()).or_insert(Bucket {
            tokens: capacity_f64,
            last_refill: now,
        });
        Self::refill(&mut entry, limit, now, capacity_f64);

        if entry.tokens + 1e-9 < cost {
            let deficit = cost - entry.tokens;
            let retry_after = if limit.refill_per_sec() <= 0.0 {
                1
            } else {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let seconds = (deficit / limit.refill_per_sec()).ceil().max(1.0) as u64;
                seconds
            };
            let reset_at = epoch_secs() + retry_after;
            return RateLimitOutcome {
                allowed: false,
                remaining: 0,
                reset_at,
                retry_after_secs: retry_after.max(1),
                capacity,
            };
        }

        entry.tokens -= cost;
        let remaining = entry.tokens;
        let secs_to_full = if limit.refill_per_sec() <= 0.0 {
            0.0
        } else {
            (capacity_f64 - remaining) / limit.refill_per_sec()
        };
        RateLimitOutcome {
            allowed: true,
            remaining: tokens_left(remaining),
            reset_at: epoch_secs() + seconds_to_full(secs_to_full),
            retry_after_secs: 0,
            capacity,
        }
    }
}

/// Tokens left in the bucket, as a whole number.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn tokens_left(tokens: f64) -> u32 {
    tokens.floor().max(0.0) as u32
}

/// Seconds until the bucket is full again, as a whole number of seconds.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn seconds_to_full(secs: f64) -> u64 {
    secs.ceil().max(0.0) as u64
}

fn epoch_secs() -> u64 {
    // `X-RateLimit-Reset` advertises an epoch instant, so it comes from the
    // wall clock; the bucket itself refills against a monotonic `Instant`.
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod rate_limit_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::domain::model::SustainedRate;
    use crate::domain::services::data_plane::RateLimitKey;
    use parking_lot::Mutex;

    /// A clock the test advances by hand.
    struct ManualClock {
        current: Mutex<Instant>,
    }

    impl ManualClock {
        fn start() -> (Arc<Self>, Instant) {
            let now = Instant::now();
            (
                Arc::new(Self {
                    current: Mutex::new(now),
                }),
                now,
            )
        }

        fn advance(&self, by: Duration) {
            *self.current.lock() += by;
        }
    }

    impl Clock for ManualClock {
        fn now(&self) -> Instant {
            *self.current.lock()
        }
    }

    fn limit(rate: u32, window: u32, burst: Option<u32>) -> RateLimit {
        RateLimit {
            sustained: SustainedRate {
                rate,
                window_secs: window,
            },
            burst_capacity: burst,
            ..RateLimit::default()
        }
    }

    #[tokio::test]
    async fn initial_burst_reaches_capacity() {
        let registry = TokenBucketRegistry::new();
        let rl = limit(1, 1, Some(3));
        for _ in 0..3 {
            let outcome = registry.acquire("k", &rl, 1).await;
            assert!(outcome.allowed);
        }
        let outcome = registry.acquire("k", &rl, 1).await;
        assert!(!outcome.allowed, "the fourth request exceeds the burst");
        assert_eq!(outcome.retry_after_secs, 1);
    }

    #[tokio::test]
    async fn exhaustion_reports_the_retry_after() {
        let (clock, _) = ManualClock::start();
        let registry = TokenBucketRegistry::with_clock(clock.clone());
        let rl = limit(2, 1, Some(1));
        assert!(registry.acquire("k", &rl, 1).await.allowed);
        let outcome = registry.acquire("k", &rl, 1).await;
        assert!(!outcome.allowed);
        assert!(outcome.retry_after_secs >= 1);
        clock.advance(Duration::from_secs(1));
        assert!(
            registry.acquire("k", &rl, 1).await.allowed,
            "a token refilled"
        );
    }

    #[tokio::test]
    async fn refill_follows_the_elapsed_time() {
        let (clock, _) = ManualClock::start();
        let registry = TokenBucketRegistry::with_clock(clock.clone());
        let rl = limit(2, 1, Some(2));
        assert_eq!(registry.acquire("k", &rl, 2).await.remaining, 0);
        clock.advance(Duration::from_millis(500));
        assert!(
            registry.acquire("k", &rl, 1).await.allowed,
            "one token refilled"
        );
        let outcome = registry.acquire("k", &rl, 1).await;
        assert!(!outcome.allowed, "only one token refilled in half a second");
    }

    #[tokio::test]
    async fn distinct_keys_hold_distinct_buckets() {
        let registry = TokenBucketRegistry::new();
        let rl = limit(1, 1, Some(1));
        assert!(registry.acquire("tenant:a", &rl, 1).await.allowed);
        assert!(registry.acquire("tenant:b", &rl, 1).await.allowed);
        assert!(!registry.acquire("tenant:a", &rl, 1).await.allowed);
        assert_eq!(registry.len(), 2);
    }

    #[tokio::test]
    async fn scope_keys_are_distinguished() {
        let tenant = uuid::Uuid::new_v4();
        let subject = uuid::Uuid::new_v4();
        assert_ne!(
            RateLimitKey::Tenant(tenant).to_string(),
            RateLimitKey::Subject(tenant, subject).to_string()
        );
        assert_eq!(
            RateLimitKey::resolve(
                &crate::domain::model::RateLimit {
                    scope: crate::domain::model::RateLimitScope::Subject,
                    ..RateLimit::default()
                },
                tenant,
                subject,
                "10.0.0.1"
            ),
            RateLimitKey::Subject(tenant, subject)
        );
        assert_eq!(
            RateLimitKey::resolve(&RateLimit::default(), tenant, subject, "10.0.0.1"),
            RateLimitKey::Tenant(tenant)
        );
        assert_eq!(
            RateLimitKey::Ip("10.0.0.1".to_owned()).with_resource("route:r1"),
            "ip:10.0.0.1:route:r1"
        );
    }

    #[tokio::test]
    async fn cost_consumes_more_than_one_token() {
        let (clock, _) = ManualClock::start();
        let registry = TokenBucketRegistry::with_clock(Arc::clone(&clock) as Arc<dyn Clock>);
        let rl = limit(1, 1, Some(4));
        let outcome = registry.acquire("k", &rl, 3).await;
        assert!(outcome.allowed);
        assert_eq!(outcome.remaining, 1);
        // The last token still buys one request; the following one is refused.
        assert!(registry.acquire("k", &rl, 1).await.allowed);
        assert!(!registry.acquire("k", &rl, 1).await.allowed);
    }

    #[tokio::test]
    async fn a_zero_rate_never_refills_but_always_reports_a_retry() {
        let (clock, _) = ManualClock::start();
        let registry = TokenBucketRegistry::with_clock(Arc::clone(&clock) as Arc<dyn Clock>);
        let rl = limit(0, 1, Some(1));
        assert!(registry.acquire("k", &rl, 1).await.allowed);
        let outcome = registry.acquire("k", &rl, 1).await;
        assert!(!outcome.allowed);
        assert_eq!(outcome.retry_after_secs, 1);
    }

    /// Tokens left, compared the way floats are compared in a test.
    fn assert_tokens(registry: &TokenBucketRegistry, key: &str, limit: &RateLimit, expected: f64) {
        let seen = registry.peek(key, limit);
        assert!(
            (seen - expected).abs() < f64::EPSILON.max(expected.abs() * 1e-9),
            "expected {expected} tokens, saw {seen}"
        );
    }

    #[tokio::test]
    async fn peek_reports_without_consuming() {
        let (clock, _) = ManualClock::start();
        let registry = TokenBucketRegistry::with_clock(Arc::clone(&clock) as Arc<dyn Clock>);
        let rl = limit(1, 1, Some(5));
        assert_tokens(&registry, "k", &rl, 5.0);
        registry.acquire("k", &rl, 2).await;
        assert_tokens(&registry, "k", &rl, 3.0);
        assert_tokens(&registry, "k", &rl, 3.0);
    }

    #[tokio::test]
    async fn a_rejection_carries_the_documented_capacity() {
        let (clock, _) = ManualClock::start();
        let registry = TokenBucketRegistry::with_clock(Arc::clone(&clock) as Arc<dyn Clock>);
        let rl = limit(1, 1, Some(7));
        for _ in 0..7 {
            registry.acquire("k", &rl, 1).await;
        }
        let outcome = registry.acquire("k", &rl, 1).await;
        assert_eq!(outcome.capacity, 7);
        assert_eq!(outcome.remaining, 0);
        assert_tokens(&registry, "k", &rl, 0.0);
    }
}
