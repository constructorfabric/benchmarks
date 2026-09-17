//! Token-bucket rate limiting (ADR-0003).
//!
//! Buckets are keyed by a scope-derived key and live in a process-local
//! cache owned by the data plane. Ancestors configured with
//! `sharing: enforce` contribute a ceiling that is applied as
//! `min(ancestor, descendant)`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;

use crate::domain::model::{RateLimit, RateScope, RateStrategy, SharingMode};
use crate::error::{ErrorKind, OagwError};

/// Outcome of a rate-limit check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateDecision {
    /// Tokens available; request may proceed.
    Allowed {
        /// Tokens left in the bucket after this request.
        remaining: u64,
    },
    /// Bucket drained; request must be rejected.
    Limited {
        /// `Retry-After` hint in seconds.
        retry_after: u64,
    },
    /// `strategy: degrade` — proceed with reduced functionality.
    Degrade,
    /// `strategy: queue` — request was admitted to the queue.
    Queued,
}

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    capacity: f64,
    last: Instant,
}

impl Bucket {
    fn new(capacity: u64) -> Self {
        Self {
            tokens: capacity as f64,
            capacity: capacity as f64,
            last: Instant::now(),
        }
    }

    fn take(&mut self, cost: u64, interval: Duration) -> bool {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        let refill = if interval.is_zero() {
            f64::INFINITY
        } else {
            elapsed / interval.as_secs_f64()
        };
        self.tokens = (self.tokens + refill).min(self.capacity);
        if self.tokens >= cost as f64 {
            self.tokens -= cost as f64;
            true
        } else {
            false
        }
    }
}

/// Cache of live token buckets.
#[derive(Debug, Default)]
pub struct RateLimiter {
    buckets: DashMap<String, Bucket>,
    queue: DashMap<String, u64>,
}

impl RateLimiter {
    /// Create an empty limiter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Check and consume `cost` tokens for `key`.
    #[must_use]
    pub fn check(&self, cfg: &RateLimit, key: &str) -> RateDecision {
        let cost = cfg.cost.max(1);
        let capacity = cfg.capacity().max(1);
        let interval = cfg.refill_interval();
        let mut entry = self.buckets.entry(key.to_owned()).or_insert_with(|| Bucket::new(capacity));
        // Re-configure in case the configuration changed between requests.
        if (entry.capacity - capacity as f64).abs() > f64::EPSILON {
            entry.capacity = capacity as f64;
        }
        let granted = entry.take(cost, interval);
        let remaining = entry.tokens.floor().max(0.0) as u64;
        drop(entry);

        if granted {
            return RateDecision::Allowed { remaining };
        }
        match cfg.strategy {
            RateStrategy::Reject => RateDecision::Limited {
                retry_after: cfg.retry_after_seconds(),
            },
            RateStrategy::Degrade => RateDecision::Degrade,
            RateStrategy::Queue => {
                let mut current = self.queue.entry(key.to_owned()).or_insert(0);
                if *current >= 128 {
                    drop(current);
                    return RateDecision::Limited {
                        retry_after: cfg.retry_after_seconds(),
                    };
                }
                *current += 1;
                drop(current);
                RateDecision::Queued
            }
        }
    }

    /// Release one queued slot for `key`.
    pub fn release(&self, key: &str) {
        if let Some(mut v) = self.queue.get_mut(key) {
            *v = v.saturating_sub(1);
        }
    }
}

/// Build the scope key for a rate limit counter.
#[must_use]
pub fn rate_key(
    upstream_id: uuid::Uuid,
    route_id: Option<uuid::Uuid>,
    scope: RateScope,
    tenant_id: uuid::Uuid,
    subject_id: uuid::Uuid,
    client_ip: Option<std::net::IpAddr>,
) -> String {
    let route_part = route_id.map(|r| r.to_string()).unwrap_or_else(|| "-".to_owned());
    let actor = match scope {
        RateScope::Global => "global".to_owned(),
        RateScope::Tenant => format!("tenant:{tenant_id}"),
        RateScope::User => format!("user:{subject_id}"),
        RateScope::Ip => format!("ip:{}", client_ip.map(|i| i.to_string()).unwrap_or_default()),
        RateScope::Route => format!("route:{route_part}"),
    };
    format!("{upstream_id}/{actor}")
}

/// Merge a hierarchical rate-limit chain into one effective limit.
///
/// Ancestors configured `enforce` always constrain (`min` of the two
/// sustained rates and bucket capacities); `inherit` ancestors only supply a
/// value when the descendant did not configure one; `private` ancestors are
/// invisible to descendants.
#[must_use]
pub fn effective_limit(
    ancestors: impl IntoIterator<Item = (RateLimit, SharingMode)>,
    local: Option<RateLimit>,
) -> Option<RateLimit> {
    let mut acc = local;
    for (ancestor, sharing) in ancestors {
        match sharing {
            SharingMode::Enforce => match &mut acc {
                Some(local) => {
                    local.sustained.rate = local.sustained.rate.min(ancestor.sustained.rate);
                    if let Some(burst) = local.burst.as_mut() {
                        burst.capacity = burst.capacity.min(ancestor.capacity());
                    }
                    if local.capacity() > ancestor.capacity() {
                        local.burst = Some(crate::domain::model::Burst {
                            capacity: ancestor.capacity(),
                        });
                    }
                }
                None => acc = Some(ancestor),
            },
            SharingMode::Inherit | SharingMode::Private => {
                if acc.is_none() {
                    acc = Some(ancestor);
                }
            }
        }
    }
    acc
}

/// Convert a rate decision into a rejection error.
#[must_use]
pub fn limited_error(decision: RateDecision) -> Option<OagwError> {
    match decision {
        RateDecision::Allowed { .. } | RateDecision::Degrade | RateDecision::Queued => None,
        RateDecision::Limited { retry_after } => Some(
            OagwError::new(
                ErrorKind::RateLimitExceeded,
                "rate limit exceeded for this scope",
            )
            .with_retry_after(retry_after.max(1)),
        ),
    }
}

/// Shared limiter handle.
pub type SharedRateLimiter = Arc<RateLimiter>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{RateAlgorithm, SustainedRate, RateWindow};

    fn cfg(rate: u64, capacity: u64) -> RateLimit {
        RateLimit {
            sharing: Default::default(),
            algorithm: RateAlgorithm::TokenBucket,
            sustained: SustainedRate {
                rate,
                window: RateWindow::Second,
            },
            burst: Some(crate::domain::model::Burst { capacity }),
            scope: RateScope::default(),
            strategy: RateStrategy::Reject,
            cost: 1,
        }
    }

    #[test]
    fn drains_bucket_then_limits() {
        let limiter = RateLimiter::new();
        let c = cfg(2, 2);
        assert!(matches!(limiter.check(&c, "k"), RateDecision::Allowed { .. }));
        assert!(matches!(limiter.check(&c, "k"), RateDecision::Allowed { .. }));
        assert!(matches!(limiter.check(&c, "k"), RateDecision::Limited { .. }));
    }

    #[test]
    fn separate_keys_are_independent() {
        let limiter = RateLimiter::new();
        let c = cfg(1, 1);
        assert!(matches!(limiter.check(&c, "a"), RateDecision::Allowed { .. }));
        assert!(matches!(limiter.check(&c, "b"), RateDecision::Allowed { .. }));
        assert!(matches!(limiter.check(&c, "a"), RateDecision::Limited { .. }));
    }

    #[test]
    fn weighted_cost_consumes_more() {
        let limiter = RateLimiter::new();
        let mut c = cfg(1, 3);
        c.cost = 3;
        assert!(matches!(limiter.check(&c, "k"), RateDecision::Allowed { .. }));
        assert!(matches!(limiter.check(&c, "k"), RateDecision::Limited { .. }));
    }

    #[test]
    fn degrade_strategy_never_rejects() {
        let limiter = RateLimiter::new();
        let mut c = cfg(1, 1);
        c.strategy = RateStrategy::Degrade;
        assert!(matches!(limiter.check(&c, "k"), RateDecision::Allowed { .. }));
        assert!(matches!(limiter.check(&c, "k"), RateDecision::Degrade));
    }

    #[test]
    fn limited_error_sets_retry_after() {
        let e = limited_error(RateDecision::Limited { retry_after: 4 }).unwrap();
        assert_eq!(e.kind, ErrorKind::RateLimitExceeded);
        assert_eq!(e.retry_after, Some(4));
    }

    #[test]
    fn rate_key_scopes_counters() {
        let t = uuid::Uuid::new_v4();
        let u = uuid::Uuid::new_v4();
        let s = uuid::Uuid::new_v4();
        assert_ne!(
            rate_key(s, None, RateScope::Tenant, t, u, None),
            rate_key(s, None, RateScope::User, t, u, None)
        );
    }
}
