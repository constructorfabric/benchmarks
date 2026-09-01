//! The token-bucket rate limiter (`ADR`-0003).
//!
//! One bucket per scope key, in process, refilled lazily on access: a check
//! never blocks and never allocates beyond the key. The bucket is sized by
//! `burst.capacity` (defaulting to the sustained rate) and refilled at
//! `sustained.rate / window`; a request costs `rate_limit.cost` tokens. A
//! rejection reports the quota in the `X-RateLimit-*` headers and the wait in
//! `Retry-After` (`ADR`-0003, More Information).

// The bucket keeps fractional tokens: rates and capacities arrive from the
// manifest as small integers, so the conversions in this module stay exact for
// every value the schema admits and the `f64` state is the point of the design.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;

use crate::domain::error::DomainError;
use crate::domain::model::{RateLimitConfig, RateScope};

/// Hard ceiling on tracked buckets, so an unbounded key space (one per client
/// IP) cannot grow the process without limit.
const MAX_BUCKETS: usize = 100_000;

/// How long an untouched bucket stays tracked before the sweeper drops it.
const IDLE_BUCKET_TTL: Duration = Duration::from_hours(1);

/// One tracked counter.
struct Bucket {
    state: std::sync::Mutex<BucketState>,
}

#[derive(Debug, Clone, Copy)]
struct BucketState {
    tokens: f64,
    last: std::time::Instant,
}

/// The refill rate and capacity a [`RateLimitConfig`] turns into.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Budget {
    capacity: f64,
    refill_per_sec: f64,
}

impl Budget {
    fn new(config: &RateLimitConfig) -> Self {
        let window_secs = match config.sustained.window {
            crate::domain::model::RateWindow::Second => 1.0,
            crate::domain::model::RateWindow::Minute => 60.0,
            crate::domain::model::RateWindow::Hour => 3_600.0,
            crate::domain::model::RateWindow::Day => 86_400.0,
        };
        // `burst.capacity` is the bucket size; it never cuts below one window
        // of sustained traffic, so a bucket can always absorb one refill.
        let capacity = config.burst.map_or(config.sustained.rate, |burst| {
            burst.capacity.max(config.sustained.rate)
        });
        Self {
            capacity: capacity as f64,
            refill_per_sec: config.sustained.rate as f64 / window_secs,
        }
    }
}

/// A verdict of one rate-limit check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateDecision {
    /// Tokens left in the bucket after the request.
    pub remaining: u64,
    /// Configured limit, for the `X-RateLimit-Limit` header.
    pub limit: u64,
    /// Seconds until the bucket is full again (`X-RateLimit-Reset`).
    pub reset: u64,
}

/// The in-process limiter of one upstream or route (`ADR`-0003).
pub struct RateLimiter {
    budget: Budget,
    cost: u64,
    scope: RateScope,
    buckets: Arc<DashMap<String, Bucket>>,
}

impl RateLimiter {
    /// A limiter for `config`.
    #[must_use]
    pub fn new(config: &RateLimitConfig) -> Self {
        Self {
            budget: Budget::new(config),
            cost: config.cost.max(1),
            scope: config.scope,
            buckets: Arc::new(DashMap::new()),
        }
    }

    /// The counter key this limiter uses, honouring the configured scope. A
    /// `global` scope ignores the identity; `route` pins the route id.
    #[must_use]
    pub fn key(
        &self,
        tenant: uuid::Uuid,
        subject: uuid::Uuid,
        client_ip: Option<&str>,
        route_id: Option<uuid::Uuid>,
    ) -> String {
        match self.scope {
            RateScope::Global => "global".to_owned(),
            RateScope::Tenant => format!("tenant:{tenant}"),
            RateScope::User => format!("user:{tenant}:{subject}"),
            RateScope::Ip => format!("ip:{}", client_ip.unwrap_or("unknown")),
            RateScope::Route => format!("route:{tenant}:{}", route_id.unwrap_or(tenant)),
        }
    }

    /// The configured limit, for the `X-RateLimit-Limit` header.
    #[must_use]
    pub fn limit(&self) -> u64 {
        self.budget.capacity.max(0.0) as u64
    }

    /// The tokens one request spends.
    #[must_use]
    pub const fn cost(&self) -> u64 {
        self.cost
    }

    /// Try to spend `self.cost` tokens from the counter of `key`.
    ///
    /// # Errors
    /// Returns [`DomainError::RateLimitExceeded`] when the bucket cannot cover
    /// the cost, carrying the `Retry-After` in seconds.
    pub fn check(&self, key: &str) -> Result<RateDecision, DomainError> {
        let now = std::time::Instant::now();
        self.sweep(now);
        let entry = self.buckets.entry(key.to_owned()).or_insert(Bucket {
            state: std::sync::Mutex::new(BucketState {
                tokens: self.budget.capacity,
                last: now,
            }),
        });
        let mut state = entry
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let elapsed = now.duration_since(state.last).as_secs_f64();
        state.tokens =
            (state.tokens + elapsed * self.budget.refill_per_sec).min(self.budget.capacity);
        state.last = now;

        if state.tokens < self.cost as f64 {
            let retry_after = self.retry_after(state.tokens);
            state.tokens = state.tokens.max(0.0);
            std::mem::drop(state);
            return Err(DomainError::RateLimitExceeded {
                detail: format!("rate limit of {} exceeded", self.limit()),
                retry_after: Some(Duration::from_secs(retry_after)),
            });
        }

        state.tokens -= self.cost as f64;
        let remaining = state.tokens;
        let reset = self.seconds_until_full(remaining);
        std::mem::drop(state);
        Ok(RateDecision {
            remaining: remaining.floor().max(0.0) as u64,
            limit: self.limit(),
            reset,
        })
    }

    /// Drop the counters that have not been touched for [`IDLE_BUCKET_TTL`].
    fn sweep(&self, now: std::time::Instant) {
        if self.buckets.len() < MAX_BUCKETS {
            return;
        }
        self.buckets.retain(|_, bucket| {
            bucket
                .state
                .lock()
                .is_ok_and(|state| now.duration_since(state.last) < IDLE_BUCKET_TTL)
        });
    }

    fn seconds_until_full(&self, tokens: f64) -> u64 {
        if self.budget.refill_per_sec <= 0.0 {
            return 0;
        }
        let missing = self.budget.capacity - tokens;
        if missing <= 0.0 {
            return 0;
        }
        (missing / self.budget.refill_per_sec).ceil() as u64
    }

    /// Seconds until the bucket can cover one more request.
    fn retry_after(&self, tokens: f64) -> u64 {
        let missing = self.cost as f64 - tokens.max(0.0);
        if self.budget.refill_per_sec <= 0.0 || missing <= 0.0 {
            return 1;
        }
        (missing / self.budget.refill_per_sec).ceil().max(1.0) as u64
    }
}
