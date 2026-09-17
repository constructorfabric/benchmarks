//! Data-plane rate limiting (ADR 0003).
//!
//! The control plane stores the *configuration*; the counters themselves are
//! data-plane state, held here in process. Two algorithms are supported, both
//! derived from the same [`RateLimitConfig`]:
//!
//! * [`RateAlgorithm::TokenBucket`] — `sustained.rate` tokens replenish evenly
//!   across `sustained.window`, up to `burst.capacity` (default: the sustained
//!   rate). Bursts up to the capacity are allowed.
//! * [`RateAlgorithm::SlidingWindow`] — at most `sustained.rate` requests in
//!   any window, which prevents boundary bursts.
//!
//! Inheritance across the tenant hierarchy is resolved before this module sees
//! the configuration ([`crate::infra::proxy::resolve`]): the effective limit is
//! `min(parent, child)`.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use dashmap::DashMap;

use crate::domain::model::{RateAlgorithm, RateLimitConfig};

/// Outcome of a rate-limit check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimitDecision {
    /// Whether the request may proceed.
    pub allowed: bool,
    /// Configured sustained rate (the advertised `X-RateLimit-Limit`).
    pub limit: u64,
    /// Requests still available in the current window.
    pub remaining: u64,
    /// Seconds until the allowance is replenished.
    pub reset_seconds: u64,
    /// Seconds the client should wait before retrying (429 responses).
    pub retry_after_seconds: u64,
    /// True when a `degrade` strategy let an over-limit request through.
    pub degraded: bool,
}

impl RateLimitDecision {
    fn allow(cfg: &RateLimitConfig, remaining: u64, reset_seconds: u64) -> Self {
        Self {
            allowed: true,
            limit: cfg.sustained.rate,
            remaining,
            reset_seconds,
            retry_after_seconds: 0,
            degraded: false,
        }
    }

    fn reject(cfg: &RateLimitConfig, retry_after_seconds: u64, reset_seconds: u64) -> Self {
        Self {
            allowed: false,
            limit: cfg.sustained.rate,
            remaining: 0,
            reset_seconds: reset_seconds.max(retry_after_seconds),
            retry_after_seconds: retry_after_seconds.max(1),
            degraded: false,
        }
    }
}

#[derive(Debug)]
enum State {
    Bucket { tokens: f64, last: Instant },
    Window { seen: VecDeque<Instant> },
}

/// Counter state for one key.
#[derive(Debug)]
struct Entry {
    state: State,
    last_used: Instant,
}

/// The rate-limit counters of the data plane.
///
/// Counters live for the lifetime of the process and are swept when idle, so a
/// churn of keys (per-IP scopes, for instance) cannot grow the map without
/// bound.
pub struct RateLimiter {
    entries: DashMap<String, Entry>,
    capacity: usize,
}

/// Default idle lifetime of a counter before the sweep drops it.
const IDLE_TTL: Duration = Duration::from_secs(600);

impl RateLimiter {
    /// Create a limiter; `capacity` bounds the number of live counters before
    /// an idle sweep is triggered eagerly.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: DashMap::with_capacity(capacity.max(16)),
            capacity: capacity.max(16),
        }
    }

    /// Number of live counters.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when no counters are live.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Drop counters that have not been touched for `idle` (or the default
    /// idle TTL when `None`).
    pub fn sweep(&self, idle: Option<Duration>) {
        let ttl = idle.unwrap_or(IDLE_TTL);
        let now = Instant::now();
        self.entries
            .retain(|_, entry| now.duration_since(entry.last_used) < ttl);
    }

    /// Consume `cfg.cost` from the counter identified by `key`.
    #[must_use]
    pub fn check(&self, key: &str, cfg: &RateLimitConfig, now: Instant) -> RateLimitDecision {
        if self.entries.len() >= self.capacity.saturating_mul(4) {
            self.sweep(None);
        }
        match cfg.algorithm {
            RateAlgorithm::TokenBucket => self.check_token_bucket(key, cfg, now),
            RateAlgorithm::SlidingWindow => self.check_sliding_window(key, cfg, now),
        }
    }

    fn check_token_bucket(
        &self,
        key: &str,
        cfg: &RateLimitConfig,
        now: Instant,
    ) -> RateLimitDecision {
        let capacity = cfg.effective_burst().max(1) as f64;
        let rate = cfg.sustained.rate.max(1);
        let window_secs = cfg.sustained.window.as_secs().max(1) as f64;
        let per_sec = rate as f64 / window_secs;
        let cost = cfg.cost.max(1) as f64;

        let mut entry = self.entries.entry(key.to_owned()).or_insert_with(|| Entry {
            state: State::Bucket {
                tokens: capacity,
                last: now,
            },
            last_used: now,
        });
        entry.last_used = now;

        let (mut tokens, mut last) = match entry.state {
            State::Bucket { tokens, last } => (tokens, last),
            // The key switched algorithm; start a fresh bucket.
            State::Window { .. } => (capacity, now),
        };

        let elapsed = now.duration_since(last).as_secs_f64();
        if elapsed > 0.0 {
            tokens = (tokens + elapsed * per_sec).min(capacity);
            last = now;
        }

        if tokens >= cost {
            tokens -= cost;
            let remaining = tokens.floor().max(0.0) as u64;
            // Seconds until a single token is back.
            let reset = if per_sec > 0.0 {
                ((cost - tokens) / per_sec).ceil().max(0.0) as u64
            } else {
                window_secs as u64
            };
            entry.state = State::Bucket { tokens, last };
            return RateLimitDecision::allow(cfg, remaining, reset);
        }
        let deficit = cost - tokens;
        let retry = if per_sec > 0.0 {
            (deficit / per_sec).ceil().max(1.0) as u64
        } else {
            window_secs as u64
        };
        let reset = if per_sec > 0.0 {
            ((capacity - tokens) / per_sec).ceil().max(0.0) as u64
        } else {
            window_secs as u64
        };
        entry.state = State::Bucket { tokens, last };
        RateLimitDecision::reject(cfg, retry, reset)
    }

    fn check_sliding_window(
        &self,
        key: &str,
        cfg: &RateLimitConfig,
        now: Instant,
    ) -> RateLimitDecision {
        let window = Duration::from_secs(cfg.sustained.window.as_secs().max(1));
        let limit = cfg.sustained.rate.max(1);
        let cost = cfg.cost.max(1);

        let mut entry = self.entries.entry(key.to_owned()).or_insert_with(|| Entry {
            state: State::Window {
                seen: VecDeque::new(),
            },
            last_used: now,
        });
        entry.last_used = now;

        let mut seen = match &entry.state {
            State::Window { seen } => seen.clone(),
            // The key switched algorithm; start a fresh window.
            State::Bucket { .. } => VecDeque::new(),
        };

        while let Some(oldest) = seen.front() {
            if now.duration_since(*oldest) >= window {
                seen.pop_front();
            } else {
                break;
            }
        }
        let used = seen.len() as u64;
        if used + cost > limit {
            let retry_after = seen
                .front()
                .map(|oldest| {
                    window
                        .checked_sub(now.duration_since(*oldest))
                        .unwrap_or(window)
                        .as_secs()
                })
                .unwrap_or(window.as_secs())
                .max(1);
            entry.state = State::Window { seen };
            return RateLimitDecision::reject(cfg, retry_after, window.as_secs());
        }
        for _ in 0..cost {
            seen.push_back(now);
        }
        let remaining = limit.saturating_sub(seen.len() as u64);
        let reset = seen
            .front()
            .map(|oldest| {
                window
                    .checked_sub(now.duration_since(*oldest))
                    .unwrap_or(Duration::ZERO)
                    .as_secs()
            })
            .unwrap_or(window.as_secs());
        entry.state = State::Window { seen };
        RateLimitDecision::allow(cfg, remaining, reset)
    }
}

/// Counter key for a rate-limit configuration (ADR 0003 scope table).
///
/// The upstream id is always part of the key: two upstreams with the same
/// scope must never share a counter.
#[must_use]
pub fn counter_key(
    upstream_id: uuid::Uuid,
    route_id: Option<uuid::Uuid>,
    scope: crate::domain::model::RateScope,
    tenant_id: uuid::Uuid,
    subject_id: uuid::Uuid,
    client_ip: Option<&str>,
) -> String {
    use crate::domain::model::RateScope as S;
    let scope_part = match scope {
        S::Global => "global".to_owned(),
        S::Tenant => format!("tenant:{tenant_id}"),
        S::User => format!("tenant:{tenant_id}/user:{subject_id}"),
        S::Ip => format!("tenant:{tenant_id}/ip:{}", client_ip.unwrap_or("unknown")),
        S::Route => format!(
            "tenant:{tenant_id}/route:{}",
            route_id.map_or_else(|| "unrouted".to_owned(), |id| id.to_string())
        ),
    };
    format!("rl:{upstream_id}:{scope_part}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{RateAlgorithm, RateLimitConfig, RateWindow};

    fn config(algorithm: RateAlgorithm, rate: u64, burst: Option<u64>) -> RateLimitConfig {
        RateLimitConfig {
            algorithm,
            sustained: crate::domain::model::SustainedRate {
                rate,
                window: RateWindow::Second,
            },
            burst: burst.map(|capacity| crate::domain::model::BurstCapacity { capacity }),
            ..RateLimitConfig::default()
        }
    }

    #[test]
    fn token_bucket_allows_bursts_then_rejects() {
        let limiter = RateLimiter::new(16);
        let cfg = config(RateAlgorithm::TokenBucket, 1, Some(3));
        let now = Instant::now();
        for _ in 0..3 {
            assert!(limiter.check("k", &cfg, now).allowed);
        }
        let decision = limiter.check("k", &cfg, now);
        assert!(!decision.allowed);
        assert_eq!(decision.retry_after_seconds, 1);
        assert_eq!(decision.limit, 1);
    }

    #[test]
    fn token_bucket_refills_over_time() {
        let limiter = RateLimiter::new(16);
        let cfg = config(RateAlgorithm::TokenBucket, 1, None);
        let start = Instant::now();
        assert!(limiter.check("k", &cfg, start).allowed);
        assert!(
            !limiter
                .check("k", &cfg, start + Duration::from_millis(10))
                .allowed
        );
        // A second has passed: one token is back.
        assert!(
            limiter
                .check("k", &cfg, start + Duration::from_secs(2))
                .allowed
        );
    }

    #[test]
    fn sliding_window_never_exceeds_the_rate() {
        let limiter = RateLimiter::new(16);
        let cfg = config(RateAlgorithm::SlidingWindow, 2, Some(5));
        let now = Instant::now();
        assert!(limiter.check("k", &cfg, now).allowed);
        assert!(limiter.check("k", &cfg, now).allowed);
        let rejected = limiter.check("k", &cfg, now);
        assert!(!rejected.allowed);
        assert_eq!(rejected.remaining, 0);
    }

    #[test]
    fn scopes_are_isolated() {
        let limiter = RateLimiter::new(16);
        let cfg = config(RateAlgorithm::SlidingWindow, 1, None);
        let now = Instant::now();
        let a = counter_key(
            uuid::Uuid::new_v4(),
            None,
            crate::domain::model::RateScope::Ip,
            uuid::Uuid::new_v4(),
            uuid::Uuid::nil(),
            Some("10.0.0.1"),
        );
        let b = counter_key(
            uuid::Uuid::new_v4(),
            None,
            crate::domain::model::RateScope::Ip,
            uuid::Uuid::new_v4(),
            uuid::Uuid::nil(),
            Some("10.0.0.2"),
        );
        assert!(limiter.check(&a, &cfg, now).allowed);
        assert!(limiter.check(&b, &cfg, now).allowed);
        assert!(!limiter.check(&a, &cfg, now).allowed);
    }

    #[test]
    fn sweep_drops_idle_counters() {
        let limiter = RateLimiter::new(16);
        let cfg = config(RateAlgorithm::TokenBucket, 1, None);
        let _ = limiter.check("k", &cfg, Instant::now() - Duration::from_secs(120));
        limiter.sweep(Some(Duration::from_secs(60)));
        assert_eq!(limiter.len(), 0);
    }
}
