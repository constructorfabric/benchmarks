//! Token-bucket rate limiting (ADR 0003).
//!
//! Buckets are owned by the data plane (ADR 0006) and keyed by scope. Replenish
//! is computed lazily on each `try_acquire`, so there is no background task.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use crate::domain::error::DomainError;
use crate::domain::model::RateLimitConfig;

/// Rounds a non-negative duration to whole seconds.
///
/// Token counts are bounded by the configured capacity (`u32`), so the result
/// stays far inside `u64`; the saturation keeps a pathological float from
/// wrapping to zero.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn to_secs(value: f64) -> u64 {
    (value.ceil().max(1.0)) as u64
}

/// Floors a non-negative token count.
///
/// Buckets never hold more than their capacity, so this cannot exceed `u32`.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn to_tokens(value: f64) -> u32 {
    (value.floor().max(0.0)) as u32
}

/// One token bucket.
struct Bucket {
    tokens: f64,
    capacity: f64,
    last_refill: std::time::Instant,
}

impl Bucket {
    fn refill(&mut self, config: &RateLimitConfig) {
        let now = std::time::Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        let per_second =
            f64::from(config.sustained.rate) / config.sustained.window.duration().as_secs_f64();
        if per_second > 0.0 {
            self.tokens = (self.tokens + per_second * elapsed).min(self.capacity);
        }
        self.last_refill = now;
    }
}

/// In-process token-bucket registry.
#[derive(Default)]
pub struct RateLimiter {
    buckets: Mutex<HashMap<String, Bucket>>,
}

/// The outcome of a rate-limit check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateDecision {
    /// Tokens remaining in the bucket after this request.
    pub remaining: u32,
    /// Seconds until the bucket has a token again (when rejected).
    pub retry_after: u64,
    /// Seconds until the bucket is fully replenished.
    pub reset: u64,
}

impl RateLimiter {
    /// Builds a limiter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Attempts to consume `config.cost` tokens from the bucket identified by
    /// `key`.
    ///
    /// # Errors
    /// Returns a [`RateRejection`] carrying the `429` error **and** the bucket
    /// state, so the response can still advertise `X-RateLimit-*`.
    pub fn try_acquire(
        &self,
        key: &str,
        config: &RateLimitConfig,
    ) -> Result<RateDecision, RateRejection> {
        let capacity = config.capacity();
        let cost = f64::from(config.cost.max(1));
        let rate = config.sustained.rate.max(1);
        let refill_secs = config.sustained.window.duration().as_secs_f64();

        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let bucket = buckets.entry(key.to_owned()).or_insert_with(|| Bucket {
            tokens: f64::from(capacity),
            capacity: f64::from(capacity),
            last_refill: std::time::Instant::now(),
        });
        bucket.capacity = f64::from(capacity);
        bucket.refill(config);
        let per_second = f64::from(rate) / refill_secs.max(1.0);

        if bucket.tokens + f64::EPSILON < cost {
            let deficit = (cost - bucket.tokens).max(0.0);
            let retry_after = to_secs(deficit / per_second);
            let decision = RateDecision {
                remaining: 0,
                retry_after: retry_after.max(1),
                reset: to_secs((bucket.capacity - bucket.tokens) / per_second),
            };
            return Err(RateRejection {
                error: DomainError::RateLimitExceeded {
                    retry_after: decision.retry_after,
                },
                decision,
            });
        }
        bucket.tokens -= cost;
        let remaining = to_tokens(bucket.tokens);
        Ok(RateDecision {
            remaining,
            retry_after: 0,
            reset: to_secs((bucket.capacity - bucket.tokens) / per_second),
        })
    }
}

/// A rejected acquisition: the `429` plus the state needed for its headers.
#[derive(Debug, Clone)]
pub struct RateRejection {
    /// The error to answer with.
    pub error: DomainError,
    /// Bucket state after the rejected attempt.
    pub decision: RateDecision,
}

impl RateRejection {
    /// `Retry-After` in seconds.
    #[must_use]
    pub fn retry_after(&self) -> u64 {
        self.decision.retry_after.max(1)
    }
}

/// Builds the limiter key for a request, honouring the configured scope.
#[must_use]
pub fn bucket_key(config: &RateLimitConfig, context: &RateScopeContext) -> String {
    match config.scope {
        crate::domain::model::RateScope::Global => "global".to_owned(),
        crate::domain::model::RateScope::Tenant => format!("tenant:{}", context.tenant_id),
        crate::domain::model::RateScope::User => format!(
            "user:{}:{}",
            context.tenant_id,
            context.user_id.clone().unwrap_or_else(|| "-".to_owned())
        ),
        crate::domain::model::RateScope::Ip => {
            format!("ip:{}", context.client_ip.as_deref().unwrap_or("-"))
        }
        crate::domain::model::RateScope::Route => {
            format!("route:{}:{}", context.tenant_id, context.route_id)
        }
    }
}

/// Identity inputs used to key a rate-limit bucket.
#[derive(Debug, Clone, Default)]
pub struct RateScopeContext {
    /// Calling tenant.
    pub tenant_id: String,
    /// Authenticated subject, when known.
    pub user_id: Option<String>,
    /// Client IP, when known.
    pub client_ip: Option<String>,
    /// Matched route id.
    pub route_id: String,
}

/// The `Retry-After` / `X-RateLimit-*` values carried on a `429` response.
#[must_use]
pub fn rate_limit_headers(
    config: &RateLimitConfig,
    decision: &RateDecision,
) -> Vec<(&'static str, String)> {
    let mut headers = Vec::new();
    if config.response_headers {
        headers.push(("x-ratelimit-limit", config.capacity().to_string()));
        headers.push(("x-ratelimit-remaining", decision.remaining.to_string()));
        headers.push(("x-ratelimit-reset", decision.reset.to_string()));
    }
    headers
}

/// Duration formatting helper for the `Retry-After` header.
#[must_use]
pub fn retry_after_header(value: Duration) -> String {
    value.as_secs().to_string()
}
