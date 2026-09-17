//! In-process rate limiter (algorithm `cpt-cf-oagw-algo-rate-limiting-cache-lookup`,
//! DoD `cpt-cf-oagw-dod-rate-limiting-bucket`,
//! `cpt-cf-oagw-dod-rate-limiting-cache`).
//!
//! [`RateLimiter`] keeps one [`Bucket`] per `(tenant_id, upstream_id,
//! route_id)` key in a [`DashMap`] and implements [`Decider`] for the Data
//! Plane.  Lazy lifecycle — `token_cache_ttl_secs` (no background sweep):
//!
//! - a bucket whose effective [`RateLimitConfig`] changed (fingerprint
//!   mismatch) is re-created on the next decision instead of reused;
//! - a bucket idle for longer than the TTL is re-created on the next decision.
//!
//! The `cost` (weight) from the config is charged per decision, and the
//! strategy ([`RateLimitStrategy`]) maps an exhausted bucket onto
//! `Allow` / `Queue` / `Degrade` / `Rejected` (`inst-rl-429-headers`).
//!
//! Buckets are only ever created lazily: a key with no traffic costs no
//! memory, and an idle key is reclaimed by the next touch of that key (the
//! DashMap entry is replaced) — a strict in-process design with no external
//! round-trip (DoD `cpt-cf-oagw-dod-rate-limiting-cache`).

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dashmap::DashMap;
use uuid::Uuid;

use crate::domain::entity::config::RateLimitStrategy;
use crate::domain::error::DomainError;
use crate::domain::rate::{Decider, RateLimitDecision, RateLimitInfo, RateLimitRequest};

/// A clock source, injectable for deterministic tests.
pub type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// Default TTL when the gear does not pin `token_cache_ttl_secs`.
pub const DEFAULT_BUCKET_TTL: Duration = Duration::from_secs(300);

/// Shared bucket key (algorithm `cpt-cf-oagw-algo-rate-limiting-effective-min`:
/// buckets are never shared across the `(tenant, upstream, route)` tuple).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RateLimitKey {
    tenant_id: Uuid,
    upstream_id: Uuid,
    route_id: Option<Uuid>,
}

/// A live bucket plus the bookkeeping needed for lazy expiry (in-process
/// cache lifecycle per DoD `cpt-cf-oagw-dod-rate-limiting-cache`).
struct BucketEntry {
    key: RateLimitKey,
    bucket: crate::domain::rate::token_bucket::Bucket,
    /// Fingerprint of the [`crate::domain::rate::token_bucket::BucketParams`]
    /// the bucket was created with; a mismatch means the effective config
    /// changed and the bucket must be re-created.
    fingerprint: u64,
    /// Last decision instant — used for the lazy TTL sweep.
    last_seen: Instant,
}

impl BucketEntry {
    fn new(
        key: RateLimitKey,
        params: &crate::domain::rate::token_bucket::BucketParams,
        fingerprint: u64,
        now: Instant,
    ) -> Self {
        Self {
            bucket: crate::domain::rate::token_bucket::Bucket::new(params, now),
            key,
            fingerprint,
            last_seen: now,
        }
    }
}

impl std::fmt::Debug for BucketEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // DashMap entry values don't need secret hygiene here, but keep the
        // debug print shallow (bucket state is uninteresting).
        f.debug_struct("BucketEntry")
            .field("key", &self.key)
            .field("fingerprint", &self.fingerprint)
            .field("last_seen", &self.last_seen)
            .finish_non_exhaustive()
    }
}

/// Thread-safe in-process rate limiter (DoD
/// `cpt-cf-oagw-dod-rate-limiting-bucket`).
///
/// Construct with [`RateLimiter::new`] (real clock) or
/// [`RateLimiter::with_clock`] (deterministic tests).  [`Decider::decide`] is
/// the only entry point the Data Plane needs.
pub struct RateLimiter {
    buckets: DashMap<RateLimitKey, BucketEntry>,
    clock: Clock,
    /// Lazy-expiry window (`token_cache_ttl_secs`); a bucket idle beyond this
    /// is re-created on its next use.
    ttl: Duration,
}

impl std::fmt::Debug for RateLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The clock closure is excluded from Debug output.
        f.debug_struct("RateLimiter")
            .field("buckets", &self.buckets)
            .field("ttl", &self.ttl)
            .finish_non_exhaustive()
    }
}

impl RateLimiter {
    /// Creates a limiter with the real clock and the given lazy TTL.
    #[must_use]
    pub fn new(ttl: Duration) -> Self {
        Self::with_clock(ttl, Arc::new(Instant::now))
    }

    /// Creates a limiter with an injected clock (tests).
    #[must_use]
    pub fn with_clock(ttl: Duration, clock: Clock) -> Self {
        Self {
            buckets: DashMap::new(),
            clock,
            ttl,
        }
    }

    /// The number of live buckets (diagnostics).
    #[must_use]
    pub fn len(&self) -> usize {
        self.buckets.len()
    }

    /// Whether the limiter holds no buckets yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }

    /// Runs one decision against the keyed bucket.
    ///
    /// The bucket is created on first use and lazily re-created when its
    /// effective config changed (fingerprint mismatch) or it sat idle longer
    /// than the TTL (`inst-rl-bucket-refill` on a fresh bucket starts full).
    fn decide_at(&self, request: &RateLimitRequest, now: Instant) -> RateLimitDecision {
        let key = RateLimitKey {
            tenant_id: request.tenant_id,
            upstream_id: request.upstream_id,
            route_id: request.route_id,
        };

        let params = crate::domain::rate::token_bucket::BucketParams::from_config(&request.config);
        let fingerprint = params.fingerprint();
        let cost = request.config.cost.max(1);

        // Lazy lifecycle: re-create on fingerprint change or TTL expiry.
        let mut entry = self
            .buckets
            .entry(key.clone())
            .or_insert_with(|| BucketEntry::new(key.clone(), &params, fingerprint, now));
        let stale =
            entry.fingerprint != fingerprint || now.duration_since(entry.last_seen) > self.ttl;
        if stale {
            *entry = BucketEntry::new(key, &params, fingerprint, now);
        }

        // Charge the bucket (steps `inst-rl-bucket-consume` /
        // `inst-rl-bucket-empty`); the active decision is taken after the
        // refill so `remaining` reflects the current state.
        let acquired = entry.bucket.try_acquire(cost, now);
        entry.last_seen = now;

        let retry_after = if acquired {
            Duration::ZERO
        } else {
            entry.bucket.time_until_available(cost, now)
        };
        let info = RateLimitInfo {
            limit: entry.bucket.capacity(),
            remaining: entry.bucket.remaining(now),
            reset_in: entry.bucket.time_until_full(now),
            retry_after,
            strategy: request.config.strategy,
        };

        match request.config.strategy {
            RateLimitStrategy::Reject => {
                if acquired {
                    RateLimitDecision::Allow(info)
                } else {
                    RateLimitDecision::Rejected(info)
                }
            }
            RateLimitStrategy::Queue => {
                if acquired {
                    RateLimitDecision::Allow(info)
                } else {
                    // Admission is granted by the bounded queue owner (p5);
                    // `retry_after` is the earliest wait for budget.
                    RateLimitDecision::Queue(info)
                }
            }
            RateLimitStrategy::Degrade => {
                if acquired {
                    RateLimitDecision::Allow(info)
                } else {
                    RateLimitDecision::Degrade(info)
                }
            }
        }
    }
}

#[async_trait]
impl Decider for RateLimiter {
    async fn decide(&self, request: &RateLimitRequest) -> RateLimitDecision {
        let now = (self.clock)();
        self.decide_at(request, now)
    }
}

/// Convenience: projects the [`RateLimitDecision`] onto the 429 gateway
/// error when rejected (algorithm `cpt-cf-oagw-algo-rate-limiting-emit-429`,
/// step `inst-rl-429-headers`).  The `X-RateLimit-*` / `Retry-After` header
/// values live on [`RateLimitInfo`].
impl RateLimitDecision {
    /// The 429 [`DomainError`] for a rejected decision, if this decision is a
    /// rejection.
    #[must_use]
    pub fn rejection_error(&self) -> Option<DomainError> {
        match self {
            Self::Rejected(info) => Some(info.exceeded_error("rate limit exceeded")),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::entity::config::{
        BurstConfig, RateLimitAlgorithm, RateLimitConfig, RateLimitScope, RateLimitStrategy,
        RateLimitWindow, SharingMode, SustainedRate,
    };
    use crate::domain::rate::token_bucket::window_seconds;

    /// A controllable clock: advances in whole seconds.
    fn fake_clock(start: Instant) -> (Clock, Arc<std::sync::atomic::AtomicU64>) {
        let offset = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let off = Arc::clone(&offset);
        let clock: Clock = Arc::new(move || {
            start + Duration::from_secs(off.load(std::sync::atomic::Ordering::SeqCst))
        });
        (clock, offset)
    }

    fn config(
        rate: u64,
        window: RateLimitWindow,
        burst: Option<u64>,
        strategy: RateLimitStrategy,
        cost: u64,
    ) -> RateLimitConfig {
        RateLimitConfig {
            sharing: SharingMode::Private,
            algorithm: RateLimitAlgorithm::TokenBucket,
            sustained: SustainedRate { rate, window },
            burst: burst.map(|capacity| BurstConfig {
                capacity: Some(capacity),
            }),
            scope: RateLimitScope::Tenant,
            strategy,
            cost,
        }
    }

    const TENANT: Uuid = Uuid::from_u128(1);
    const UPSTREAM: Uuid = Uuid::from_u128(2);
    const ROUTE: Uuid = Uuid::from_u128(3);

    fn request(config: &RateLimitConfig, route_id: Option<Uuid>) -> RateLimitRequest {
        RateLimitRequest {
            tenant_id: TENANT,
            upstream_id: UPSTREAM,
            route_id,
            config: config.clone(),
        }
    }

    #[tokio::test]
    async fn allows_up_to_burst_then_rejects_with_429_info() {
        let (clock, _offset) = fake_clock(Instant::now());
        let limiter = RateLimiter::with_clock(Duration::from_secs(300), clock);
        let cfg = config(
            2,
            RateLimitWindow::Second,
            Some(5),
            RateLimitStrategy::Reject,
            1,
        );
        let req = request(&cfg, Some(ROUTE));

        for i in 0..5 {
            match limiter.decide(&req).await {
                RateLimitDecision::Allow(info) => {
                    assert_eq!(info.limit, 5, "burst {i} allowed");
                    assert_eq!(info.remaining, 5 - i - 1);
                }
                other => panic!("burst slot {i} must allow, got {other:?}"),
            }
        }
        let rejected = limiter.decide(&req).await;
        match rejected {
            RateLimitDecision::Rejected(info) => {
                assert_eq!(info.limit, 5);
                assert_eq!(info.remaining, 0);
                // 1 token at 2/s → 0.5s → ceil 1s.
                assert_eq!(info.retry_after, Duration::from_secs(1));
                assert_eq!(info.reset_in, Duration::from_secs(3), "full in 2.5s → 3s");
                let err = rejected.rejection_error().expect("rejected carries error");
                assert_eq!(err.status(), 429);
                assert_eq!(
                    err.instance(),
                    "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1"
                );
                assert_eq!(err.retry_after(), Some(Duration::from_secs(1)));
            }
            other => panic!("expected reject, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn min_effective_rate_is_enforced_per_upstream_route_tuple() {
        let (clock, offset) = fake_clock(Instant::now());
        let limiter = RateLimiter::with_clock(Duration::from_secs(300), clock);
        // The effective (min of upstream and route) config is what feeds
        // `decide` — algorithm `cpt-cf-oagw-algo-rate-limiting-effective-min`,
        // merged by the DP in `crate::domain::merge`.  Here the effective
        // config is the tighter 1/s no-burst window.
        let upstream_cfg = config(
            1,
            RateLimitWindow::Second,
            None,
            RateLimitStrategy::Reject,
            1,
        );
        let req = request(&upstream_cfg, Some(ROUTE));
        assert!(matches!(
            limiter.decide(&req).await,
            RateLimitDecision::Allow(_)
        ));
        assert!(matches!(
            limiter.decide(&req).await,
            RateLimitDecision::Rejected(_)
        ));

        // A different route tuple has its own bucket: not throttled by the
        // other route's consumption.
        let other = request(&upstream_cfg, Some(Uuid::from_u128(99)));
        assert!(matches!(
            limiter.decide(&other).await,
            RateLimitDecision::Allow(_)
        ));

        // After the window the original tuple recovers.
        offset.store(
            window_seconds(RateLimitWindow::Second),
            std::sync::atomic::Ordering::SeqCst,
        );
        assert!(matches!(
            limiter.decide(&req).await,
            RateLimitDecision::Allow(_)
        ));
    }

    #[tokio::test]
    async fn queue_and_degrade_strategies_surface_exhaustion() {
        let (clock, _offset) = fake_clock(Instant::now());
        let limiter = RateLimiter::with_clock(Duration::from_secs(300), clock);

        let queue_cfg = config(
            1,
            RateLimitWindow::Second,
            None,
            RateLimitStrategy::Queue,
            1,
        );

        let q_req = request(&queue_cfg, Some(ROUTE));
        assert!(matches!(
            limiter.decide(&q_req).await,
            RateLimitDecision::Allow(_)
        ));
        match limiter.decide(&q_req).await {
            RateLimitDecision::Queue(info) => {
                assert_eq!(info.retry_after, Duration::from_secs(1));
                assert!(
                    limiter.decide(&q_req).await.rejection_error().is_none(),
                    "Queue is not a 429 rejection"
                );
            }
            other => panic!("expected Queue, got {other:?}"),
        }

        let degrade_cfg = config(
            1,
            RateLimitWindow::Second,
            None,
            RateLimitStrategy::Degrade,
            1,
        );
        let d_req = request(&degrade_cfg, Some(Uuid::from_u128(77)));
        assert!(matches!(
            limiter.decide(&d_req).await,
            RateLimitDecision::Allow(_)
        ));
        assert!(matches!(
            limiter.decide(&d_req).await,
            RateLimitDecision::Degrade(_)
        ));
    }

    #[tokio::test]
    async fn lazy_expiry_respects_ttl_and_config_change() {
        let (clock, offset) = fake_clock(Instant::now());
        let limiter = RateLimiter::with_clock(Duration::from_secs(300), clock);
        let cfg = config(
            10,
            RateLimitWindow::Second,
            None,
            RateLimitStrategy::Reject,
            1,
        );
        let req = request(&cfg, Some(ROUTE));

        // Drain the bucket.
        for _ in 0..10 {
            assert!(matches!(
                limiter.decide(&req).await,
                RateLimitDecision::Allow(_)
            ));
        }
        assert!(matches!(
            limiter.decide(&req).await,
            RateLimitDecision::Rejected(_)
        ));

        // Idle beyond the TTL → the next use re-creates a full bucket.
        offset.store(301, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(
            limiter.decide(&req).await,
            RateLimitDecision::Allow(_)
        ));

        // A config change re-creates the bucket mid-life: new rate 1/s with no
        // burst must be drained instantly.
        let tight = config(
            1,
            RateLimitWindow::Second,
            None,
            RateLimitStrategy::Reject,
            1,
        );
        let tight_req = request(&tight, Some(ROUTE));
        assert!(matches!(
            limiter.decide(&tight_req).await,
            RateLimitDecision::Allow(_)
        ));
        match limiter.decide(&tight_req).await {
            RateLimitDecision::Rejected(info) => assert_eq!(info.limit, 1),
            other => panic!("tight config must reject second call, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn sliding_window_variant_limits_within_the_window() {
        let (clock, offset) = fake_clock(Instant::now());
        let limiter = RateLimiter::with_clock(Duration::from_secs(300), clock);
        let cfg = RateLimitConfig {
            algorithm: RateLimitAlgorithm::SlidingWindow,
            ..config(
                2,
                RateLimitWindow::Second,
                None,
                RateLimitStrategy::Reject,
                1,
            )
        };
        let req = request(&cfg, None);
        // Capacity 2 → two in-window requests allowed, a third rejected.
        for _ in 0..2 {
            assert!(matches!(
                limiter.decide(&req).await,
                RateLimitDecision::Allow(_)
            ));
        }
        assert!(matches!(
            limiter.decide(&req).await,
            RateLimitDecision::Rejected(_)
        ));
        // A second later the oldest request expired → budget frees up.
        offset.store(1, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(
            limiter.decide(&req).await,
            RateLimitDecision::Allow(_)
        ));
    }

    #[tokio::test]
    async fn weighted_cost_charges_more_than_one_token() {
        let (clock, _offset) = fake_clock(Instant::now());
        let limiter = RateLimiter::with_clock(Duration::from_secs(300), clock);
        // Bucket of 5 but each request costs 3 → two requests take 6 > 5.
        let cfg = config(
            5,
            RateLimitWindow::Second,
            Some(5),
            RateLimitStrategy::Reject,
            3,
        );
        let req = request(&cfg, Some(ROUTE));
        let first = limiter.decide(&req).await;
        assert!(matches!(first, RateLimitDecision::Allow(_)));
        let second = limiter.decide(&req).await;
        match second {
            RateLimitDecision::Rejected(info) => {
                assert_eq!(info.remaining, 2, "3 of 5 consumed by the first call");
            }
            other => panic!("weighted cost must exhaust, got {other:?}"),
        }
    }
}
