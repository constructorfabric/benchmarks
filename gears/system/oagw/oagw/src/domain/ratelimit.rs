//! Per-instance rate limiting (`ADR/0003-rate-limiting.md`).
//!
//! MVP scope is local state only — the Redis sync layer described in §4 of
//! the ADR does not exist, so counters are per process. Both documented
//! algorithms are implemented: a token bucket (bursty, the default) and a
//! sliding window (no boundary burst).

use parking_lot::Mutex;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use dashmap::DashMap;

use super::model::{RateAlgorithm, RateLimitConfig, RateScope, RateStrategy};

/// Outcome of a limiter check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateDecision {
    /// Whether the request may proceed.
    pub allowed: bool,
    /// Configured tokens per window.
    pub limit: u32,
    /// Tokens left after this check.
    pub remaining: u32,
    /// Seconds until enough tokens exist for another request of this cost.
    pub retry_after_secs: u64,
    /// Whether the caller should mark the request as degraded rather than
    /// reject it (`strategy: degrade`).
    pub degraded: bool,
}

impl RateDecision {
    /// An unconditional allow — used when no limit is configured.
    #[must_use]
    pub const fn unlimited() -> Self {
        Self {
            allowed: true,
            limit: u32::MAX,
            remaining: u32::MAX,
            retry_after_secs: 0,
            degraded: false,
        }
    }
}

/// Classic token bucket. `tokens` and `capacity` are floats so that sustained
/// rates below one token per second refill smoothly.
#[derive(Debug)]
struct TokenBucket {
    tokens: f64,
    capacity: f64,
    refill_per_sec: f64,
    last_update: Instant,
}

impl TokenBucket {
    fn new(capacity: f64, refill_per_sec: f64, now: Instant) -> Self {
        Self {
            tokens: capacity,
            capacity,
            refill_per_sec,
            last_update: now,
        }
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last_update).as_secs_f64();
        if elapsed > 0.0 {
            self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
            self.last_update = now;
        }
    }

    fn try_acquire(&mut self, cost: f64, now: Instant) -> (bool, f64, f64) {
        self.refill(now);
        if self.tokens >= cost {
            self.tokens -= cost;
            (true, self.tokens, 0.0)
        } else {
            let deficit = cost - self.tokens;
            let wait = if self.refill_per_sec > 0.0 {
                deficit / self.refill_per_sec
            } else {
                f64::from(u16::MAX)
            };
            (false, self.tokens, wait)
        }
    }
}

/// Sliding window counter: request timestamps within the trailing window.
#[derive(Debug)]
struct SlidingWindow {
    hits: Vec<(Instant, u32)>,
    window: Duration,
    limit: u32,
}

impl SlidingWindow {
    fn new(limit: u32, window: Duration) -> Self {
        Self {
            hits: Vec::new(),
            window,
            limit,
        }
    }

    fn try_acquire(&mut self, cost: u32, now: Instant) -> (bool, u32, f64) {
        let cutoff = now.checked_sub(self.window).unwrap_or(now);
        self.hits.retain(|(at, _)| *at > cutoff);
        let used: u32 = self.hits.iter().map(|(_, c)| *c).sum();
        if used.saturating_add(cost) <= self.limit {
            self.hits.push((now, cost));
            (true, self.limit - used - cost, 0.0)
        } else {
            // The oldest hit leaving the window is the earliest retry point.
            let wait = self.hits.first().map_or(self.window.as_secs_f64(), |(at, _)| {
                self.window
                    .saturating_sub(now.saturating_duration_since(*at))
                    .as_secs_f64()
            });
            (false, self.limit.saturating_sub(used), wait)
        }
    }
}

#[derive(Debug)]
enum Limiter {
    Bucket(TokenBucket),
    Window(SlidingWindow),
}

/// Identity of a rate-limit counter.
///
/// The `{resource_type}:{resource_id}` prefix mirrors the Redis key layout in
/// `ADR/0003-rate-limiting.md` §4 so a resource's counters can be dropped by
/// prefix when it is deleted.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RateKey(String);

impl RateKey {
    /// Build the counter key for `(resource, scope)`.
    #[must_use]
    pub fn new(resource_type: &str, resource_id: &str, scope: RateScope, scope_id: &str) -> Self {
        let scope_name = match scope {
            RateScope::Global => "global",
            RateScope::Tenant => "tenant",
            RateScope::User => "user",
            RateScope::Ip => "ip",
            RateScope::Route => "route",
        };
        Self(format!(
            "oagw:ratelimit:{resource_type}:{resource_id}:{scope_name}:{scope_id}"
        ))
    }

    /// Prefix shared by every counter belonging to a resource.
    #[must_use]
    pub fn resource_prefix(resource_type: &str, resource_id: &str) -> String {
        format!("oagw:ratelimit:{resource_type}:{resource_id}:")
    }

    /// Underlying key string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Registry of live limiters, owned by the Data Plane (ADR 0006).
#[derive(Debug, Default)]
pub struct RateLimiterRegistry {
    limiters: DashMap<String, Mutex<Limiter>>,
}

impl RateLimiterRegistry {
    /// Empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            limiters: DashMap::new(),
        }
    }

    /// Evaluate `config` for `key`, consuming `config.cost` tokens on success.
    ///
    /// `strategy: queue` is not a blocking wait here — the caller decides
    /// whether to await [`RateDecision::retry_after_secs`]; `degrade` allows
    /// the request through with [`RateDecision::degraded`] set.
    #[must_use]
    pub fn check(&self, key: &RateKey, config: &RateLimitConfig) -> RateDecision {
        self.check_at(key, config, Instant::now())
    }

    /// [`Self::check`] with an injected clock, for deterministic tests.
    #[must_use]
    pub fn check_at(&self, key: &RateKey, config: &RateLimitConfig, now: Instant) -> RateDecision {
        let capacity = config.capacity();
        let cost = config.cost.max(1);
        let entry = self.limiters.entry(key.0.clone()).or_insert_with(|| {
            Mutex::new(match config.algorithm {
                RateAlgorithm::TokenBucket => Limiter::Bucket(TokenBucket::new(
                    f64::from(capacity),
                    config.sustained.per_second(),
                    now,
                )),
                RateAlgorithm::SlidingWindow => Limiter::Window(SlidingWindow::new(
                    config.sustained.rate,
                    Duration::from_secs(config.sustained.window.as_secs()),
                )),
            })
        });

        let (allowed, remaining, wait) = {
            let mut guard = entry.lock();
            match &mut *guard {
                Limiter::Bucket(bucket) => {
                    // A merged configuration can tighten the limit after the
                    // bucket was created; keep the live bucket in step.
                    bucket.capacity = f64::from(capacity);
                    bucket.refill_per_sec = config.sustained.per_second();
                    bucket.tokens = bucket.tokens.min(bucket.capacity);
                    let (ok, tokens, wait) = bucket.try_acquire(f64::from(cost), now);
                    (ok, tokens.max(0.0) as u32, wait)
                }
                Limiter::Window(window) => {
                    window.limit = config.sustained.rate;
                    let (ok, remaining, wait) = window.try_acquire(cost, now);
                    (ok, remaining, wait)
                }
            }
        };

        let retry_after_secs = wait.ceil().max(1.0) as u64;
        let degraded = !allowed && config.strategy == RateStrategy::Degrade;
        RateDecision {
            allowed: allowed || degraded,
            limit: config.sustained.rate,
            remaining,
            retry_after_secs: if allowed { 0 } else { retry_after_secs },
            degraded,
        }
    }

    /// Drop every counter belonging to a resource — called when an upstream
    /// or route is deleted so a recreated resource starts clean.
    pub fn purge_resource(&self, resource_type: &str, resource_id: &str) {
        let prefix = RateKey::resource_prefix(resource_type, resource_id);
        self.limiters.retain(|key, _| !key.starts_with(&prefix));
    }

    /// Number of live counters (diagnostics and tests).
    #[must_use]
    pub fn len(&self) -> usize {
        self.limiters.len()
    }

    /// Whether no counters are live.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.limiters.is_empty()
    }
}

/// `X-RateLimit-*` headers for a decision, per the IETF draft referenced by
/// `ADR/0003-rate-limiting.md`.
#[must_use]
pub fn rate_limit_headers(decision: &RateDecision) -> HashMap<&'static str, String> {
    let mut headers = HashMap::new();
    headers.insert("x-ratelimit-limit", decision.limit.to_string());
    headers.insert("x-ratelimit-remaining", decision.remaining.to_string());
    headers.insert("x-ratelimit-reset", decision.retry_after_secs.to_string());
    headers
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{BurstConfig, RateWindow, SharingMode, SustainedRate};

    fn cfg(rate: u32, capacity: Option<u32>, algorithm: RateAlgorithm) -> RateLimitConfig {
        RateLimitConfig {
            sharing: SharingMode::Private,
            algorithm,
            sustained: SustainedRate {
                rate,
                window: RateWindow::Second,
            },
            burst: BurstConfig { capacity },
            budget: None,
            scope: RateScope::Tenant,
            strategy: RateStrategy::Reject,
            cost: 1,
            response_headers: true,
        }
    }

    fn key() -> RateKey {
        RateKey::new("upstream", "u1", RateScope::Tenant, "t1")
    }

    #[test]
    fn token_bucket_allows_burst_up_to_capacity() {
        let reg = RateLimiterRegistry::new();
        let config = cfg(1, Some(5), RateAlgorithm::TokenBucket);
        let now = Instant::now();
        for i in 0..5 {
            assert!(
                reg.check_at(&key(), &config, now).allowed,
                "burst token {i} should be allowed"
            );
        }
        let denied = reg.check_at(&key(), &config, now);
        assert!(!denied.allowed);
        assert!(denied.retry_after_secs >= 1);
    }

    #[test]
    fn token_bucket_refills_over_time() {
        let reg = RateLimiterRegistry::new();
        let config = cfg(2, Some(2), RateAlgorithm::TokenBucket);
        let start = Instant::now();
        assert!(reg.check_at(&key(), &config, start).allowed);
        assert!(reg.check_at(&key(), &config, start).allowed);
        assert!(!reg.check_at(&key(), &config, start).allowed);
        let later = start + Duration::from_secs(1);
        assert!(reg.check_at(&key(), &config, later).allowed);
    }

    #[test]
    fn sliding_window_has_no_boundary_burst() {
        let reg = RateLimiterRegistry::new();
        let config = cfg(2, None, RateAlgorithm::SlidingWindow);
        let start = Instant::now();
        assert!(reg.check_at(&key(), &config, start).allowed);
        assert!(reg.check_at(&key(), &config, start).allowed);
        assert!(!reg.check_at(&key(), &config, start).allowed);
        // Half a window later the earlier hits are still counted.
        let mid = start + Duration::from_millis(500);
        assert!(!reg.check_at(&key(), &config, mid).allowed);
        let after = start + Duration::from_millis(1_100);
        assert!(reg.check_at(&key(), &config, after).allowed);
    }

    #[test]
    fn cost_is_charged_per_request() {
        let reg = RateLimiterRegistry::new();
        let mut config = cfg(10, Some(10), RateAlgorithm::TokenBucket);
        config.cost = 10;
        let now = Instant::now();
        assert!(reg.check_at(&key(), &config, now).allowed);
        assert!(!reg.check_at(&key(), &config, now).allowed);
    }

    #[test]
    fn degrade_strategy_allows_but_marks() {
        let reg = RateLimiterRegistry::new();
        let mut config = cfg(1, Some(1), RateAlgorithm::TokenBucket);
        config.strategy = RateStrategy::Degrade;
        let now = Instant::now();
        assert!(reg.check_at(&key(), &config, now).allowed);
        let second = reg.check_at(&key(), &config, now);
        assert!(second.allowed);
        assert!(second.degraded);
    }

    #[test]
    fn counters_are_scoped_independently() {
        let reg = RateLimiterRegistry::new();
        let config = cfg(1, Some(1), RateAlgorithm::TokenBucket);
        let now = Instant::now();
        let a = RateKey::new("upstream", "u1", RateScope::Tenant, "tenant-a");
        let b = RateKey::new("upstream", "u1", RateScope::Tenant, "tenant-b");
        assert!(reg.check_at(&a, &config, now).allowed);
        assert!(reg.check_at(&b, &config, now).allowed);
        assert!(!reg.check_at(&a, &config, now).allowed);
    }

    #[test]
    fn purge_drops_only_the_named_resource() {
        let reg = RateLimiterRegistry::new();
        let config = cfg(1, Some(1), RateAlgorithm::TokenBucket);
        let now = Instant::now();
        let _ = reg.check_at(
            &RateKey::new("upstream", "u1", RateScope::Tenant, "t"),
            &config,
            now,
        );
        let _ = reg.check_at(
            &RateKey::new("upstream", "u2", RateScope::Tenant, "t"),
            &config,
            now,
        );
        assert_eq!(reg.len(), 2);
        reg.purge_resource("upstream", "u1");
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn tightened_limit_applies_to_a_live_bucket() {
        let reg = RateLimiterRegistry::new();
        let loose = cfg(100, Some(100), RateAlgorithm::TokenBucket);
        let now = Instant::now();
        assert!(reg.check_at(&key(), &loose, now).allowed);
        let tight = cfg(1, Some(1), RateAlgorithm::TokenBucket);
        assert!(reg.check_at(&key(), &tight, now).allowed);
        assert!(!reg.check_at(&key(), &tight, now).allowed);
    }

    #[test]
    fn headers_report_limit_and_remaining() {
        let decision = RateDecision {
            allowed: false,
            limit: 100,
            remaining: 0,
            retry_after_secs: 30,
            degraded: false,
        };
        let headers = rate_limit_headers(&decision);
        assert_eq!(headers["x-ratelimit-limit"], "100");
        assert_eq!(headers["x-ratelimit-remaining"], "0");
        assert_eq!(headers["x-ratelimit-reset"], "30");
    }
}
