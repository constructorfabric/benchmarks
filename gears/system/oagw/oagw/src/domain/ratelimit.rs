//! Rate limiting (ADR 0003).
//!
//! ADR 0003 chooses a token bucket as the default algorithm with a sliding
//! window as the alternative, and hierarchical budget allocation: a
//! descendant's effective limit is the *strictest* of its own limit and every
//! `enforce`d ancestor limit. Both algorithms live here; the hierarchy merge
//! lives in [`crate::domain::service`], which hands this module the already
//! merged rule.

use std::time::Duration;

use dashmap::DashMap;

use crate::domain::model::{RateLimitAlgorithm, RateLimitRule};

/// The verdict for one request against one bucket.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Decision {
    /// The request is admitted.
    Allowed {
        /// Tokens left in the bucket after this request.
        remaining: u64,
        /// The bucket's capacity, as configured.
        limit: u64,
        /// Seconds until the bucket is full again.
        reset_seconds: u64,
    },
    /// The request is refused.
    Limited {
        /// Seconds until a request of this cost would be admitted again.
        retry_after_seconds: u64,
    },
}

impl Decision {
    /// `true` when the request is admitted.
    #[must_use]
    pub const fn allowed(&self) -> bool {
        matches!(self, Self::Allowed { .. })
    }
}

#[derive(Debug, Clone, Copy)]
struct TokenBucket {
    tokens: f64,
    last_refill_ms: u64,
    capacity: f64,
    refill_per_ms: f64,
}

impl TokenBucket {
    fn try_acquire(&mut self, now_ms: u64, cost: u64) -> (bool, u64, u64) {
        self.refill(now_ms);
        let admitted = self.tokens >= cost as f64;
        if admitted {
            self.tokens -= cost as f64;
        }
        let reset = if self.tokens >= self.capacity || self.refill_per_ms <= 0.0 {
            0
        } else {
            let missing = self.capacity - self.tokens;
            (missing / self.refill_per_ms).ceil() as u64
        };
        (admitted, self.tokens as u64, reset)
    }

    fn refill(&mut self, now_ms: u64) {
        let elapsed = now_ms.saturating_sub(self.last_refill_ms) as f64;
        if elapsed > 0.0 && self.refill_per_ms > 0.0 {
            self.tokens = (self.tokens + elapsed * self.refill_per_ms).min(self.capacity);
        }
        self.last_refill_ms = now_ms;
    }

    /// Seconds until `cost` tokens would be available again.
    fn retry_after(&mut self, now_ms: u64, cost: u64) -> u64 {
        self.refill(now_ms);
        let deficit = cost as f64 - self.tokens;
        if deficit <= 0.0 || self.refill_per_ms <= 0.0 {
            return 1;
        }
        ((deficit / self.refill_per_ms).ceil() as u64).max(1)
    }
}

#[derive(Debug, Clone, Copy)]
struct SlidingWindow {
    /// Epoch milliseconds of the window's opening edge.
    window_start_ms: u64,
    /// Tokens accounted for inside the window.
    used: u64,
}

/// In-process rate-limit buckets, keyed by the rule's scope.
///
/// ADR 0003 adopts a hybrid distribution: buckets are local and authoritative
/// per instance, with an out-of-band sync layer. This release implements the
/// local half — the gateway is a single process, so there is nothing to sync
/// with yet.
#[derive(Debug, Default)]
pub struct RateLimiter {
    buckets: DashMap<String, State>,
}

/// A shared handle to the limiter.
pub type SharedRateLimiter = std::sync::Arc<RateLimiter>;

#[derive(Debug)]
enum State {
    Token(TokenBucket),
    Sliding(SlidingWindow),
}

impl RateLimiter {
    /// An empty limiter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Evaluate `rule` against the bucket named by `key`.
    ///
    /// The key is derived by the caller from the rule's scope, so the limiter
    /// itself stays scope-agnostic. `now_ms` is injected so tests can drive
    /// time.
    #[must_use]
    pub fn check(&self, key: &str, rule: &RateLimitRule, now_ms: u64) -> Decision {
        let capacity = rule.capacity().max(1);
        match rule.algorithm {
            RateLimitAlgorithm::TokenBucket => {
                let refill_per_ms = rule.refill_per_second() / 1000.0;
                let mut entry = self.buckets.entry(key.to_owned()).or_insert_with(|| {
                    State::Token(TokenBucket {
                        tokens: capacity as f64,
                        last_refill_ms: now_ms,
                        capacity: capacity as f64,
                        refill_per_ms,
                    })
                });
                let State::Token(bucket) = entry.value_mut() else {
                    return Decision::Limited {
                        retry_after_seconds: 1,
                    };
                };
                // A reconfigured rule invalidates the shape of a live bucket.
                if (bucket.capacity - capacity as f64).abs() > f64::EPSILON
                    || (bucket.refill_per_ms - refill_per_ms).abs() > f64::EPSILON
                {
                    *bucket = TokenBucket {
                        tokens: capacity as f64,
                        last_refill_ms: now_ms,
                        capacity: capacity as f64,
                        refill_per_ms,
                    };
                }
                let (admitted, remaining, reset) = bucket.try_acquire(now_ms, rule.cost.max(1));
                let retry_after = bucket.retry_after(now_ms, rule.cost.max(1));
                drop(entry);
                if admitted {
                    Decision::Allowed {
                        remaining,
                        limit: capacity,
                        reset_seconds: reset,
                    }
                } else {
                    Decision::Limited {
                        retry_after_seconds: retry_after.max(1),
                    }
                }
            }
            RateLimitAlgorithm::SlidingWindow => {
                let window_ms = window_ms(rule);
                let limit = rule.sustained.rate.max(1);
                let mut entry = self.buckets.entry(key.to_owned()).or_insert_with(|| {
                    State::Sliding(SlidingWindow {
                        window_start_ms: now_ms,
                        used: 0,
                    })
                });
                let State::Sliding(window) = entry.value_mut() else {
                    return Decision::Limited {
                        retry_after_seconds: 1,
                    };
                };
                if now_ms.saturating_sub(window.window_start_ms) >= window_ms {
                    window.window_start_ms = now_ms;
                    window.used = 0;
                }
                let elapsed = now_ms.saturating_sub(window.window_start_ms);
                if window.used + rule.cost.max(1) > limit {
                    let retry = window_ms.saturating_sub(elapsed).div_ceil(1000).max(1);
                    Decision::Limited {
                        retry_after_seconds: retry,
                    }
                } else {
                    window.used += rule.cost.max(1);
                    Decision::Allowed {
                        remaining: limit - window.used,
                        limit,
                        reset_seconds: (window_ms - elapsed) / 1000,
                    }
                }
            }
        }
    }

    /// Drop every bucket. Used by tests.
    pub fn clear(&self) {
        self.buckets.clear();
    }
}

fn window_ms(rule: &RateLimitRule) -> u64 {
    rule.sustained.window.duration().as_millis() as u64
}

/// Build the bucket key for a rule's scope.
///
/// `route_id` is only consulted for [`crate::domain::model::RateLimitScope::Route`];
/// the caller passes `None` when no route matched.
#[must_use]
pub fn bucket_key(
    rule: &RateLimitRule,
    upstream_id: uuid::Uuid,
    route_id: Option<uuid::Uuid>,
    tenant_id: uuid::Uuid,
    subject_id: uuid::Uuid,
    client_ip: &str,
) -> String {
    use crate::domain::model::RateLimitScope;
    let owner = match rule.scope {
        RateLimitScope::Global => "global".to_owned(),
        RateLimitScope::Tenant => format!("tenant:{tenant_id}"),
        RateLimitScope::User => format!("user:{subject_id}"),
        RateLimitScope::Ip => format!("ip:{client_ip}"),
        RateLimitScope::Route => format!("route:{}", route_id.unwrap_or(upstream_id)),
    };
    format!("oagw:rl:{owner}:{upstream_id}")
}

/// The `Retry-After` implied by a decision, bounded below by one second.
#[must_use]
pub fn retry_after(decision: &Decision) -> Option<u64> {
    match decision {
        Decision::Allowed { .. } => None,
        Decision::Limited {
            retry_after_seconds,
        } => Some((*retry_after_seconds).max(1)),
    }
}

/// Milliseconds since the Unix epoch.
#[must_use]
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or_default()
}

/// A `Duration` helper for callers that need a `Duration` rather than seconds.
#[must_use]
pub const fn duration_from_seconds(seconds: u64) -> Duration {
    Duration::from_secs(seconds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{
        Burst, RateLimitScope, RateLimitStrategy, RateLimitWindow, Sharing, Sustained,
    };

    fn rule(
        algorithm: RateLimitAlgorithm,
        rate: u64,
        window: RateLimitWindow,
        capacity: Option<u64>,
    ) -> RateLimitRule {
        RateLimitRule {
            sharing: Sharing::Enforce,
            algorithm,
            sustained: Sustained { rate, window },
            burst: capacity.map(|capacity| Burst { capacity }),
            scope: RateLimitScope::Tenant,
            strategy: RateLimitStrategy::Reject,
            cost: 1,
        }
    }

    #[test]
    fn a_token_bucket_allows_bursts_up_to_capacity() {
        let limiter = RateLimiter::new();
        let config = rule(
            RateLimitAlgorithm::TokenBucket,
            1,
            RateLimitWindow::Second,
            Some(3),
        );
        assert!(limiter.check("k", &config, 0).allowed());
        assert!(limiter.check("k", &config, 0).allowed());
        assert!(limiter.check("k", &config, 0).allowed());
        assert!(!limiter.check("k", &config, 0).allowed());
    }

    #[test]
    fn a_token_bucket_refills_over_time() {
        let limiter = RateLimiter::new();
        // 2 tokens/second, capacity 1.
        let config = rule(
            RateLimitAlgorithm::TokenBucket,
            2,
            RateLimitWindow::Second,
            Some(1),
        );
        assert!(limiter.check("k", &config, 0).allowed());
        assert!(!limiter.check("k", &config, 0).allowed());
        assert!(limiter.check("k", &config, 500).allowed());
    }

    #[test]
    fn a_limited_bucket_reports_a_retry_after() {
        let limiter = RateLimiter::new();
        let config = rule(
            RateLimitAlgorithm::TokenBucket,
            1,
            RateLimitWindow::Minute,
            Some(1),
        );
        assert!(limiter.check("k", &config, 0).allowed());
        let decision = limiter.check("k", &config, 0);
        match decision {
            Decision::Limited {
                retry_after_seconds,
            } => assert!(retry_after_seconds >= 1),
            Decision::Allowed { .. } => panic!("expected the bucket to be empty"),
        }
    }

    #[test]
    fn a_token_bucket_admits_again_after_a_full_refill() {
        let limiter = RateLimiter::new();
        let config = rule(
            RateLimitAlgorithm::TokenBucket,
            1,
            RateLimitWindow::Second,
            Some(1),
        );
        assert!(limiter.check("k", &config, 0).allowed());
        assert!(!limiter.check("k", &config, 100).allowed());
        assert!(limiter.check("k", &config, 1_100).allowed());
    }

    #[test]
    fn a_sliding_window_counts_within_its_window() {
        let limiter = RateLimiter::new();
        let config = rule(
            RateLimitAlgorithm::SlidingWindow,
            2,
            RateLimitWindow::Second,
            None,
        );
        assert!(limiter.check("k", &config, 0).allowed());
        assert!(limiter.check("k", &config, 10).allowed());
        assert!(!limiter.check("k", &config, 20).allowed());
        // A fresh window admits again.
        assert!(limiter.check("k", &config, 1_100).allowed());
    }

    #[test]
    fn a_sliding_window_reports_the_remaining_window() {
        let limiter = RateLimiter::new();
        let config = rule(
            RateLimitAlgorithm::SlidingWindow,
            1,
            RateLimitWindow::Second,
            None,
        );
        assert!(limiter.check("k", &config, 0).allowed());
        match limiter.check("k", &config, 250) {
            Decision::Limited {
                retry_after_seconds,
            } => assert_eq!(retry_after_seconds, 1),
            Decision::Allowed { .. } => panic!("expected the window to be exhausted"),
        }
    }

    #[test]
    fn cost_is_charged_per_request() {
        let limiter = RateLimiter::new();
        let mut config = rule(
            RateLimitAlgorithm::TokenBucket,
            1,
            RateLimitWindow::Second,
            Some(4),
        );
        config.cost = 2;
        assert!(limiter.check("k", &config, 0).allowed());
        assert!(limiter.check("k", &config, 0).allowed());
        assert!(!limiter.check("k", &config, 0).allowed());
    }

    #[test]
    fn scoped_keys_do_not_interfere() {
        let limiter = RateLimiter::new();
        let config = rule(
            RateLimitAlgorithm::TokenBucket,
            1,
            RateLimitWindow::Second,
            Some(1),
        );
        assert!(limiter.check("ip:1", &config, 0).allowed());
        assert!(limiter.check("ip:2", &config, 0).allowed());
    }

    #[test]
    fn bucket_keys_are_scope_specific() {
        let tenant = uuid::Uuid::nil();
        let upstream = uuid::Uuid::nil();
        let mut config = rule(
            RateLimitAlgorithm::TokenBucket,
            1,
            RateLimitWindow::Second,
            None,
        );
        config.scope = RateLimitScope::Ip;
        let ip_key = bucket_key(&config, upstream, None, tenant, tenant, "10.0.0.1");
        config.scope = RateLimitScope::Route;
        let route_key = bucket_key(&config, upstream, Some(tenant), tenant, tenant, "10.0.0.1");
        config.scope = RateLimitScope::User;
        let user_key = bucket_key(&config, upstream, None, tenant, tenant, "10.0.0.1");
        config.scope = RateLimitScope::Global;
        let global_key = bucket_key(&config, upstream, None, tenant, tenant, "10.0.0.1");
        assert_ne!(ip_key, route_key);
        assert_ne!(ip_key, user_key);
        assert_ne!(user_key, global_key);
    }

    #[test]
    fn retry_after_is_absent_for_allowed_requests() {
        assert!(
            retry_after(&Decision::Allowed {
                remaining: 1,
                limit: 1,
                reset_seconds: 0
            })
            .is_none()
        );
        assert_eq!(
            retry_after(&Decision::Limited {
                retry_after_seconds: 0
            }),
            Some(1)
        );
    }

    #[test]
    fn durations_convert() {
        assert_eq!(duration_from_seconds(5), Duration::from_secs(5));
    }
}
