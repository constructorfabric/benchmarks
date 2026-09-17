//! In-memory rate limiting (ADR `0003-rate-limiting`).
//!
//! Two algorithms, both driven by the dual-rate configuration
//! ([`RateLimitConfig`]): a **token bucket** (default — allows bursts up to
//! `burst.capacity`) and a **sliding window** (no boundary bursts). Counters
//! are per-instance, as the ADR's MVP prescribes ("Hybrid Local + Periodic
//! Sync" is a later phase), and are keyed by the configured
//! [`RateLimitScope`] so the *same* limit can be shared globally, per tenant,
//! per user, per client IP or per route.
//!
//! Rate limiting executes in the data plane over the configuration resolved
//! from the control plane: the effective policy is the **stricter** of the
//! upstream's and the matched route's ([`RateLimitConfig::stricter_of`]).
//!
//! The rates involved are small integers taken from configuration, so the
//! `u64 → f64` conversions here are exact.
#![allow(clippy::cast_precision_loss)]

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use uuid::Uuid;

use crate::domain::models::{
    RateLimitAlgorithm, RateLimitConfig, RateLimitScope, RateLimitStrategy,
};

/// Upper bound on the number of tracked counters.
///
/// Every `(algorithm, scope, scope_id)` triple gets one; without a cap a
/// per-IP policy behind a large NAT would grow the map without limit. At the
/// cap the counters are dropped, which only *loosens* enforcement — it can
/// never lock a caller out.
const MAX_TRACKED_COUNTERS: usize = 100_000;

/// Outcome of one rate-limit check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitDecision {
    /// Whether the request may proceed.
    pub allowed: bool,
    /// Effective limit, for `X-RateLimit-Limit`.
    pub limit: u64,
    /// Capacity left after this request, for `X-RateLimit-Remaining`.
    pub remaining: u64,
    /// Seconds until the bucket/window is fully replenished.
    pub reset_secs: u64,
    /// When to retry, for `Retry-After` on a `429`.
    pub retry_after: Option<Duration>,
    /// `true` when the request exceeded the limit but the configured strategy
    /// was `degrade` — the request proceeds and the response is tagged.
    pub degraded: bool,
}

impl RateLimitDecision {
    fn allow(limit: u64, remaining: u64, reset_secs: u64) -> Self {
        Self {
            allowed: true,
            limit,
            remaining,
            reset_secs,
            retry_after: None,
            degraded: false,
        }
    }

    fn refuse(limit: u64, reset_secs: u64, retry_after: Duration) -> Self {
        Self {
            allowed: false,
            limit,
            remaining: 0,
            reset_secs,
            retry_after: Some(retry_after),
            degraded: false,
        }
    }
}

/// Token-bucket state (ADR 0003, "Token Bucket Algorithm").
#[derive(Debug)]
struct TokenBucket {
    tokens: f64,
    capacity: f64,
    refill_per_sec: f64,
    updated: Instant,
}

impl TokenBucket {
    /// Consumes `cost` tokens, refilling first.
    ///
    /// Returns whether the request fits and how long the caller would have to
    /// wait for `cost` tokens.
    fn take(&mut self, cost: f64, now: Instant) -> (bool, Duration) {
        let elapsed = now.saturating_duration_since(self.updated).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        self.updated = now;
        let wait_for = |missing: f64| {
            if self.refill_per_sec > 0.0 {
                Duration::from_secs_f64((missing / self.refill_per_sec).max(0.001))
            } else {
                Duration::from_secs(1)
            }
        };
        if self.tokens >= cost {
            self.tokens -= cost;
            (true, wait_for(cost))
        } else {
            (false, wait_for(cost - self.tokens))
        }
    }
}

/// Sliding-window state (ADR 0003, "Sliding Window Algorithm").
#[derive(Debug)]
struct SlidingWindow {
    hits: VecDeque<Instant>,
    window: Duration,
    limit: u64,
}

impl SlidingWindow {
    /// Consumes `cost` slots inside the window.
    fn take(&mut self, cost: u64, now: Instant) -> (bool, Duration) {
        let horizon = now.checked_sub(self.window).unwrap_or(now);
        while self.hits.front().is_some_and(|oldest| *oldest < horizon) {
            self.hits.pop_front();
        }
        let used = self.hits.len() as u64;
        if used + cost > self.limit {
            // The oldest slot leaves the window first.
            let wait = self
                .hits
                .front()
                .and_then(|oldest| oldest.checked_add(self.window))
                .map_or(self.window, |expiry| {
                    expiry.saturating_duration_since(now).max(Duration::from_millis(1))
                });
            (false, wait)
        } else {
            for _ in 0..cost {
                self.hits.push_back(now);
            }
            (true, self.window)
        }
    }
}

/// One counter, whichever algorithm the policy picked.
#[derive(Debug)]
enum Counter {
    Bucket(TokenBucket),
    Window(SlidingWindow),
}

/// Per-request rate limiting over the resolved configuration.
#[derive(Debug, Default)]
pub struct RateLimiter {
    counters: DashMap<String, Counter>,
}

impl RateLimiter {
    /// New, empty limiter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies one request against `config`, keyed by `scope_id`.
    ///
    /// A `queue` strategy is reported exactly like `reject` here; the proxy
    /// service is the component that can afford to wait, so it re-checks
    /// until its own deadline.
    pub fn check(
        &self,
        config: &RateLimitConfig,
        scope_id: &str,
        cost: u64,
        now: Instant,
    ) -> RateLimitDecision {
        let key = format!(
            "{}:{}:{}",
            algorithm_tag(config.algorithm),
            config.scope_key(),
            scope_id
        );

        // Bound the counter set before inserting a new key.
        if self.counters.len() >= MAX_TRACKED_COUNTERS && !self.counters.contains_key(&key) {
            self.counters.clear();
        }

        let capacity = config.burst_capacity().max(1);
        let window_secs = config.sustained.window.seconds().max(1);
        let refill_per_sec = config.sustained.rate.max(1) as f64 / window_secs as f64;
        let cost = cost.max(1);

        let mut counter = self
            .counters
            .entry(key)
            .or_insert_with(|| match config.algorithm {
                RateLimitAlgorithm::TokenBucket => Counter::Bucket(TokenBucket {
                    tokens: capacity as f64,
                    capacity: capacity as f64,
                    refill_per_sec,
                    updated: now,
                }),
                RateLimitAlgorithm::SlidingWindow => Counter::Window(SlidingWindow {
                    hits: VecDeque::new(),
                    window: Duration::from_secs(window_secs),
                    limit: capacity,
                }),
            });

        let (allowed, wait, limit, remaining) = match &mut *counter {
            Counter::Bucket(bucket) => {
                let (allowed, wait) = bucket.take(cost as f64, now);
                (allowed, wait, bucket.capacity as u64, bucket.tokens as u64)
            }
            Counter::Window(window) => {
                let (allowed, wait) = window.take(cost, now);
                (
                    allowed,
                    wait,
                    window.limit,
                    window.limit.saturating_sub(window.hits.len() as u64),
                )
            }
        };
        let reset_secs = wait.as_secs().max(1);
        drop(counter);

        match (allowed, config.strategy) {
            (true, _) => RateLimitDecision::allow(limit, remaining, reset_secs),
            (false, RateLimitStrategy::Reject | RateLimitStrategy::Queue) => {
                RateLimitDecision::refuse(limit, reset_secs, wait)
            }
            (false, RateLimitStrategy::Degrade) => RateLimitDecision {
                allowed: true,
                limit,
                remaining,
                reset_secs,
                retry_after: None,
                degraded: true,
            },
        }
    }

    /// Drops the counters of one resource (an upstream was deleted).
    pub fn forget_containing(&self, needle: &str) {
        self.counters.retain(|key, _| !key.contains(needle));
    }
}

/// Resolves the counter scope id of a proxied request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimitIdentity<'a> {
    /// Calling tenant.
    pub tenant_id: Uuid,
    /// Authenticated subject, when the transport carried one.
    pub subject_id: Option<Uuid>,
    /// Matched route, when the request went through route resolution.
    pub route_id: Option<Uuid>,
    /// Client IP, when the transport could determine one.
    pub client_ip: Option<&'a str>,
}

impl RateLimitIdentity<'_> {
    /// Scope id the configured [`RateLimitScope`] selects.
    ///
    /// A scope the transport could not observe (no subject, no client IP, no
    /// route) degrades to the tenant, so an unresolvable scope never widens
    /// the limit to "everyone".
    #[must_use]
    pub fn scope_id(&self, scope: RateLimitScope) -> String {
        match scope {
            RateLimitScope::Global => "global".to_owned(),
            RateLimitScope::Tenant => self.tenant_id.to_string(),
            RateLimitScope::User => self.subject_id.unwrap_or(self.tenant_id).to_string(),
            RateLimitScope::Ip => self
                .client_ip
                .map_or_else(|| self.tenant_id.to_string(), str::to_owned),
            RateLimitScope::Route => self
                .route_id
                .map_or_else(|| self.tenant_id.to_string(), |id| id.to_string()),
        }
    }
}

impl RateLimitConfig {
    /// Stable tag of the configured scope, part of every counter key.
    #[must_use]
    pub fn scope_key(&self) -> &'static str {
        match self.scope {
            RateLimitScope::Global => "global",
            RateLimitScope::Tenant => "tenant",
            RateLimitScope::User => "user",
            RateLimitScope::Ip => "ip",
            RateLimitScope::Route => "route",
        }
    }
}

fn algorithm_tag(algorithm: RateLimitAlgorithm) -> &'static str {
    match algorithm {
        RateLimitAlgorithm::TokenBucket => "token_bucket",
        RateLimitAlgorithm::SlidingWindow => "sliding_window",
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::models::{BurstCapacity, RateWindow, SustainedRate};

    fn config(algorithm: RateLimitAlgorithm, strategy: RateLimitStrategy) -> RateLimitConfig {
        RateLimitConfig {
            sharing: Default::default(),
            algorithm,
            sustained: SustainedRate {
                rate: 2,
                window: RateWindow::Second,
            },
            burst: Some(BurstCapacity { capacity: 4 }),
            scope: RateLimitScope::Global,
            strategy,
            cost: 1,
        }
    }

    #[test]
    fn token_bucket_allows_a_burst_up_to_capacity() {
        let limiter = RateLimiter::new();
        let config = config(RateLimitAlgorithm::TokenBucket, RateLimitStrategy::Reject);
        let now = Instant::now();
        for _ in 0..4 {
            assert!(limiter.check(&config, "global", 1, now).allowed);
        }
        let fifth = limiter.check(&config, "global", 1, now);
        assert!(!fifth.allowed, "{fifth:?}");
        assert!(fifth.retry_after.is_some());
    }

    #[test]
    fn token_bucket_refills_over_time() {
        let limiter = RateLimiter::new();
        let config = config(RateLimitAlgorithm::TokenBucket, RateLimitStrategy::Reject);
        let now = Instant::now();
        for _ in 0..4 {
            let _ = limiter.check(&config, "global", 1, now);
        }
        assert!(!limiter.check(&config, "global", 1, now).allowed);
        // One refill period later two tokens are back.
        let later = now + Duration::from_millis(1100);
        assert!(limiter.check(&config, "global", 1, later).allowed);
    }

    #[test]
    fn sliding_window_never_exceeds_the_limit() {
        let limiter = RateLimiter::new();
        let config = RateLimitConfig {
            algorithm: RateLimitAlgorithm::SlidingWindow,
            burst: None,
            ..config(RateLimitAlgorithm::SlidingWindow, RateLimitStrategy::Reject)
        };
        let now = Instant::now();
        // Capacity defaults to the sustained rate (2).
        assert!(limiter.check(&config, "global", 1, now).allowed);
        assert!(limiter.check(&config, "global", 1, now).allowed);
        assert!(!limiter.check(&config, "global", 1, now).allowed);
    }

    #[test]
    fn scopes_isolate_their_counters() {
        let limiter = RateLimiter::new();
        let mut config = config(RateLimitAlgorithm::TokenBucket, RateLimitStrategy::Reject);
        config.scope = RateLimitScope::Tenant;
        let now = Instant::now();
        let identity = RateLimitIdentity {
            tenant_id: Uuid::new_v4(),
            subject_id: None,
            route_id: None,
            client_ip: None,
        };
        assert!(
            limiter
                .check(&config, &identity.scope_id(config.scope), 1, now)
                .allowed
        );
        let other = RateLimitIdentity {
            tenant_id: Uuid::new_v4(),
            ..identity
        };
        assert!(
            limiter
                .check(&config, &other.scope_id(config.scope), 1, now)
                .allowed
        );
    }

    #[test]
    fn cost_is_honoured() {
        let limiter = RateLimiter::new();
        let config = RateLimitConfig {
            cost: 4,
            ..config(RateLimitAlgorithm::TokenBucket, RateLimitStrategy::Reject)
        };
        let now = Instant::now();
        let decision = limiter.check(&config, "global", 4, now);
        assert!(decision.allowed, "{decision:?}");
        assert!(!limiter.check(&config, "global", 4, now).allowed);
    }

    #[test]
    fn degrade_allows_and_flags() {
        let limiter = RateLimiter::new();
        let config = config(RateLimitAlgorithm::TokenBucket, RateLimitStrategy::Degrade);
        let now = Instant::now();
        for _ in 0..5 {
            let _ = limiter.check(&config, "global", 1, now);
        }
        let decision = limiter.check(&config, "global", 1, now);
        assert!(decision.allowed);
        assert!(decision.degraded);
    }

    #[test]
    fn scope_ids_fall_back_to_the_tenant() {
        let tenant = Uuid::nil();
        let identity = RateLimitIdentity {
            tenant_id: tenant,
            subject_id: None,
            route_id: None,
            client_ip: Some("10.0.0.1"),
        };
        assert_eq!(identity.scope_id(RateLimitScope::Global), "global");
        assert_eq!(identity.scope_id(RateLimitScope::Tenant), tenant.to_string());
        assert_eq!(identity.scope_id(RateLimitScope::User), tenant.to_string());
        assert_eq!(identity.scope_id(RateLimitScope::Ip), "10.0.0.1");
        assert_eq!(identity.scope_id(RateLimitScope::Route), tenant.to_string());
    }

    #[test]
    fn counters_are_dropped_by_resource() {
        let limiter = RateLimiter::new();
        let config = config(RateLimitAlgorithm::TokenBucket, RateLimitStrategy::Reject);
        let now = Instant::now();
        let _ = limiter.check(&config, "upstream:1", 1, now);
        limiter.forget_containing("upstream:1");
        // A fresh counter means the burst capacity is available again.
        assert!(limiter.check(&config, "upstream:1", 1, now).allowed);
    }
}
