//! Token-bucket rate limiting with the dual-rate configuration (ADR-0003).
//!
//! The bucket is local to this process: MVP per-instance enforcement with no
//! distributed coordination. Counters are keyed by the resolved limit's scope
//! so a `tenant`-scoped limit and a `route`-scoped limit never share state.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;

use crate::domain::model::RateLimitConfig;

/// Outcome of a single admission decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admission {
    /// Whether the request may proceed.
    pub allowed: bool,
    /// Effective bucket capacity (the `X-RateLimit-Limit` value).
    pub limit: u32,
    /// Tokens left in the bucket after the decision.
    pub remaining: u32,
    /// Epoch seconds at which the bucket is replenished.
    pub reset_at: u64,
    /// Seconds the client should wait before retrying.
    pub retry_after_seconds: u64,
    /// Set when a `degrade` strategy admitted the request anyway.
    pub degraded: bool,
}

/// A single token bucket.
#[derive(Debug, Clone)]
pub struct TokenBucket {
    tokens: f64,
    capacity: f64,
    refill_per_second: f64,
    last_refill: Instant,
}

impl TokenBucket {
    /// Bucket for `limit`, starting full.
    #[must_use]
    pub fn new(limit: &RateLimitConfig, now: Instant) -> Self {
        let capacity = f64::from(limit.capacity().max(1));
        Self {
            tokens: capacity,
            capacity,
            refill_per_second: limit.tokens_per_second(),
            last_refill: now,
        }
    }

    /// Add tokens for the time elapsed since the last refill.
    fn refill(&mut self, now: Instant) {
        let elapsed = now
            .saturating_duration_since(self.last_refill)
            .as_secs_f64();
        if elapsed > 0.0 {
            self.tokens = (self.tokens + elapsed * self.refill_per_second).min(self.capacity);
            self.last_refill = now;
        }
    }

    /// Seconds until `cost` tokens are available.
    fn seconds_until_available(&self, cost: u32) -> f64 {
        let missing = f64::from(cost) - self.tokens;
        if missing <= 0.0 || self.refill_per_second <= 0.0 {
            0.0
        } else {
            missing / self.refill_per_second
        }
    }

    /// Try to take `cost` tokens, refilling first.
    fn try_acquire(&mut self, cost: u32, now: Instant) -> bool {
        self.refill(now);
        let cost = f64::from(cost.max(1));
        if self.tokens >= cost {
            self.tokens -= cost;
            true
        } else {
            false
        }
    }

    /// Tokens currently in the bucket.
    ///
    /// A token count is non-negative and bounded by the configured capacity,
    /// so flooring is the intended conversion.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn remaining(&self) -> u32 {
        self.tokens.floor().max(0.0) as u32
    }
}

/// Scope key of a counter, derived from the resolved limit and the request.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ScopeKey {
    /// Logical scope (`tenant`, `user`, `ip`, `route`, `global`).
    pub scope: String,
    /// Identity the counter is keyed by.
    pub identity: String,
    /// Resource the limit came from (upstream or route id).
    pub resource: String,
}

/// Process-local rate limiter.
#[derive(Debug, Default)]
pub struct RateLimiter {
    buckets: DashMap<ScopeKey, TokenBucket>,
}

impl RateLimiter {
    /// Create an empty limiter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop every bucket belonging to `resource` (used on delete).
    pub fn forget_resource(&self, resource: &str) {
        self.buckets.retain(|key, _| key.resource != resource);
    }

    /// Run one admission decision for `limit`.
    ///
    /// `queue` strategies wait up to [`QUEUE_WAIT`] for a token before they
    /// give up; `degrade` strategies always admit and mark the request.
    pub async fn check(
        &self,
        limit: &RateLimitConfig,
        scope: &ScopeKey,
        now: Instant,
    ) -> Admission {
        let capacity = limit.capacity();
        let mut entry = self
            .buckets
            .entry(scope.clone())
            .or_insert_with(|| TokenBucket::new(limit, now));
        let bucket = entry.value_mut();
        bucket.refill(now);
        let seconds_until_available = bucket.seconds_until_available(limit.cost);
        let reset_at = epoch_seconds(now).saturating_add(whole_seconds(seconds_until_available));
        let admitted = bucket.try_acquire(limit.cost, now);
        let remaining = bucket.remaining();
        let retry_in = seconds_until_available.ceil().max(1.0);
        drop(entry);

        match (limit.strategy, admitted) {
            (_, true) => Admission {
                allowed: true,
                limit: capacity,
                remaining,
                reset_at,
                retry_after_seconds: 0,
                degraded: false,
            },
            (crate::domain::model::RateLimitStrategy::Degrade, false) => Admission {
                allowed: true,
                limit: capacity,
                remaining,
                reset_at,
                retry_after_seconds: 0,
                degraded: true,
            },
            (crate::domain::model::RateLimitStrategy::Queue, false) => {
                let waited = self.wait_for_token(scope, limit.cost).await;
                Admission {
                    allowed: waited,
                    limit: capacity,
                    remaining: self.remaining(scope, now),
                    reset_at,
                    retry_after_seconds: u64::from(!waited),
                    degraded: false,
                }
            }
            (crate::domain::model::RateLimitStrategy::Reject, false) => Admission {
                allowed: false,
                limit: capacity,
                remaining,
                reset_at,
                retry_after_seconds: retry_seconds(retry_in),
                degraded: false,
            },
        }
    }

    /// Bounded wait for a token; `false` when the wait budget is exhausted.
    async fn wait_for_token(&self, scope: &ScopeKey, cost: u32) -> bool {
        let deadline = Instant::now() + QUEUE_WAIT;
        loop {
            tokio::time::sleep(QUEUE_TICK).await;
            let now = Instant::now();
            let Some(mut entry) = self.buckets.get_mut(scope) else {
                return true;
            };
            let granted = entry.value_mut().try_acquire(cost, now);
            if granted || now >= deadline {
                return granted;
            }
        }
    }

    fn remaining(&self, scope: &ScopeKey, now: Instant) -> u32 {
        self.buckets.get(scope).map_or(0, |bucket| {
            let mut bucket = bucket.clone();
            bucket.refill(now);
            bucket.remaining()
        })
    }
}

/// Whole seconds in a non-negative duration.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn whole_seconds(value: f64) -> u64 {
    value.max(0.0) as u64
}

/// Whole seconds a client should wait, never fewer than one.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn retry_seconds(value: f64) -> u64 {
    value.ceil().max(1.0) as u64
}

/// How long a `queue` strategy waits for a token before giving up.
pub const QUEUE_WAIT: Duration = Duration::from_millis(250);
/// Poll interval used while queueing.
pub const QUEUE_TICK: Duration = Duration::from_millis(10);

fn epoch_seconds(_now: Instant) -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Build the scope key for a resolved limit.
#[must_use]
pub fn scope_key(
    limit: &crate::domain::model::RateLimitConfig,
    resource: &str,
    tenant: uuid::Uuid,
    subject: uuid::Uuid,
    client_ip: Option<&str>,
    route: Option<uuid::Uuid>,
) -> ScopeKey {
    use crate::domain::model::RateLimitScope;
    let identity = match limit.scope {
        RateLimitScope::Global => "global".to_owned(),
        RateLimitScope::Tenant => tenant.to_string(),
        RateLimitScope::User => format!("{tenant}:{subject}"),
        RateLimitScope::Ip => client_ip.unwrap_or("unknown").to_owned(),
        RateLimitScope::Route => route.map_or_else(|| resource.to_owned(), |id| id.to_string()),
    };
    ScopeKey {
        scope: format!("{:?}", limit.scope).to_ascii_lowercase(),
        identity,
        resource: resource.to_owned(),
    }
}

/// Response headers describing the remaining budget (ADR-0003).
#[must_use]
pub fn rate_limit_headers(
    admission: &Admission,
    config: &RateLimitConfig,
) -> HashMap<String, String> {
    let mut headers = HashMap::new();
    if config.response_headers {
        headers.insert("X-RateLimit-Limit".to_owned(), admission.limit.to_string());
        headers.insert(
            "X-RateLimit-Remaining".to_owned(),
            admission.remaining.to_string(),
        );
        headers.insert(
            "X-RateLimit-Reset".to_owned(),
            admission.reset_at.to_string(),
        );
    }
    if admission.degraded {
        headers.insert("X-OAGW-Rate-Limited".to_owned(), "degraded".to_owned());
    }
    headers
}

/// Shared handle for the process-wide limiter.
#[must_use]
pub fn shared_limiter() -> Arc<RateLimiter> {
    Arc::new(RateLimiter::new())
}

#[cfg(test)]
#[path = "rate_limit_tests.rs"]
mod rate_limit_tests;
