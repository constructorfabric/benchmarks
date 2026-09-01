//! In-process rate limiting (DESIGN §4.2 `oagw_rate_limit_*`, ADR 0003
//! "Distribution: Hybrid Local + Periodic Sync").
//!
//! The MVP is **local-only**: every instance keeps its own counters, which is
//! the documented degradation ("Per-instance rate limiting in Data Plane, no
//! distributed coordination"). The registry owns one limiter per
//! `(scope, scope_key, effective parameters)` tuple; the effective parameters
//! are part of the key so that a configuration change starts a fresh bucket
//! instead of silently inheriting the previous budget.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::domain::models::{RateLimitAlgorithm, RateLimitConfig, RateLimitScope};
use crate::domain::rate_limit::{
    RateLimitDecision, RateLimitParameters, SlidingWindow, TokenBucket,
};

/// One limiter instance, selected by the configured algorithm.
#[derive(Debug)]
enum Limiter {
    TokenBucket(TokenBucket),
    SlidingWindow(Box<SlidingWindow>),
}

impl Limiter {
    fn try_consume(&mut self, now_ms: u64) -> RateLimitDecision {
        match self {
            Self::TokenBucket(bucket) => bucket.try_consume(now_ms),
            Self::SlidingWindow(window) => window.try_consume(now_ms),
        }
    }
}

/// Key of one limiter, composed of the counting scope and the effective
/// parameters (ADR 0003 "Hierarchical Budget Allocation").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimiterKey {
    scope: RateLimitScope,
    scope_key: String,
    algorithm: RateLimitAlgorithm,
    capacity: u64,
    refill_per_second: u64,
}

impl LimiterKey {
    /// Builds the key of a limiter for `scope_key` (tenant, subject, IP or
    /// route identifier).
    #[must_use]
    pub fn new(config: &RateLimitConfig, scope_key: &str) -> Self {
        let parameters = RateLimitParameters::from_config(config);
        Self {
            scope: parameters.scope,
            scope_key: scope_key.to_owned(),
            algorithm: parameters.algorithm,
            capacity: parameters.capacity,
            refill_per_second: parameters.refill_per_second,
        }
    }

    /// Stable in-process map key.
    fn storage_key(&self) -> String {
        format!(
            "{:?}|{}|{:?}|{}|{}",
            self.scope, self.scope_key, self.algorithm, self.capacity, self.refill_per_second
        )
    }
}

/// Registry of the rate limiters of one process.
#[derive(Debug)]
pub struct LimiterRegistry {
    limiters: Mutex<HashMap<String, Limiter>>,
}

impl Default for LimiterRegistry {
    fn default() -> Self {
        Self {
            limiters: Mutex::new(HashMap::new()),
        }
    }
}

impl LimiterRegistry {
    /// Empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies one rate-limit check, creating the limiter on first use.
    #[must_use]
    pub fn check(&self, config: &RateLimitConfig, key: &LimiterKey, now_ms: u64) -> RateLimitDecision {
        let parameters = RateLimitParameters::from_config(config);
        let mut limiters = self
            .limiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let limiter = limiters
            .entry(key.storage_key())
            .or_insert_with(|| Self::build(&parameters, config, now_ms));
        limiter.try_consume(now_ms)
    }

    fn build(
        parameters: &RateLimitParameters,
        config: &RateLimitConfig,
        now_ms: u64,
    ) -> Limiter {
        match parameters.algorithm {
            RateLimitAlgorithm::TokenBucket => {
                Limiter::TokenBucket(TokenBucket::new(parameters, now_ms))
            }
            RateLimitAlgorithm::SlidingWindow => {
                Limiter::SlidingWindow(Box::new(SlidingWindow::new(parameters, config)))
            }
        }
    }
}

#[cfg(test)]
#[path = "ratelimit_tests.rs"]
mod tests;
