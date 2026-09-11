//! Token-bucket rate limiting.
//!
//! Realizes `cpt-cf-oagw-algo-tp-rate-limit-evaluation`,
//! `cpt-cf-oagw-dod-tp-rate-limit-token-bucket` and
//! `cpt-cf-oagw-state-tp-*` bucket lifecycle.
//!
//! Counters are per gear instance in this configuration; the distributed
//! synchronisation ADR-0003 describes is deliberately deferred.

use std::collections::HashMap;
use std::time::Instant;

use parking_lot::Mutex;

use crate::domain::model::{RateLimit, RateScope, RateStrategy};

/// Outcome of charging a bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    /// Whether the request may proceed.
    pub allowed: bool,
    /// Configured ceiling, for `X-RateLimit-Limit`.
    pub limit: u32,
    /// Whole tokens left, for `X-RateLimit-Remaining`.
    pub remaining: u32,
    /// Seconds until the bucket is full again, for `X-RateLimit-Reset`.
    pub reset_secs: u64,
    /// Seconds to wait before retrying, for `Retry-After`. Only meaningful
    /// when `allowed` is false.
    pub retry_after_secs: u64,
}

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    capacity: f64,
    refill_per_sec: f64,
    last: Instant,
}

impl Bucket {
    fn new(rl: &RateLimit, now: Instant) -> Self {
        let capacity = f64::from(rl.capacity());
        Self {
            tokens: capacity,
            capacity,
            refill_per_sec: rl.refill_per_sec().max(f64::MIN_POSITIVE),
            last: now,
        }
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        if elapsed > 0.0 {
            self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
            self.last = now;
        }
    }

    fn try_acquire(&mut self, cost: f64, now: Instant) -> bool {
        self.refill(now);
        if self.tokens >= cost {
            self.tokens -= cost;
            true
        } else {
            false
        }
    }

    /// Seconds until the bucket holds `want` tokens again.
    fn secs_until(&self, want: f64) -> u64 {
        if self.tokens >= want {
            return 0;
        }
        let deficit = want - self.tokens;
        (deficit / self.refill_per_sec).ceil().max(0.0) as u64
    }
}

/// Per-instance registry of token buckets, keyed by scope.
#[derive(Debug, Default)]
pub struct Limiter {
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl Limiter {
    /// A new, empty limiter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Charge the bucket identified by `key` under the effective limit.
    ///
    /// `queue` and `degrade` are accepted configuration values that resolve to
    /// `reject` semantics here, so the decision is the same for all three.
    // @cpt-begin:cpt-cf-oagw-dod-tp-rate-limit-token-bucket:p1:inst-full
    pub fn check(&self, key: &str, rl: &RateLimit) -> Decision {
        self.check_at(key, rl, Instant::now())
    }
    // @cpt-end:cpt-cf-oagw-dod-tp-rate-limit-token-bucket:p1:inst-full

    /// `check`, with the clock supplied, so behaviour over time is testable.
    pub fn check_at(&self, key: &str, rl: &RateLimit, now: Instant) -> Decision {
        let cost = f64::from(rl.cost.max(1));
        let mut g = self.buckets.lock();
        let bucket = g
            .entry(key.to_owned())
            .or_insert_with(|| Bucket::new(rl, now));
        // A changed configuration reshapes the live bucket rather than
        // silently keeping the old ceiling.
        let capacity = f64::from(rl.capacity());
        if (bucket.capacity - capacity).abs() > f64::EPSILON {
            bucket.capacity = capacity;
            bucket.tokens = bucket.tokens.min(capacity);
        }
        bucket.refill_per_sec = rl.refill_per_sec().max(f64::MIN_POSITIVE);

        let allowed = bucket.try_acquire(cost, now);
        let _ = rl.strategy; // queue and degrade resolve to reject semantics
        Decision {
            allowed,
            limit: rl.capacity(),
            remaining: bucket.tokens.floor().max(0.0) as u32,
            reset_secs: bucket.secs_until(bucket.capacity),
            retry_after_secs: if allowed { 0 } else { bucket.secs_until(cost).max(1) },
        }
    }
}

/// Build the counter key for a scope.
#[must_use]
pub fn scope_key(
    scope: RateScope,
    tenant: &str,
    subject: Option<&str>,
    client_ip: Option<&str>,
    route: Option<&str>,
    upstream: &str,
) -> String {
    match scope {
        RateScope::Global => "global".to_owned(),
        RateScope::Tenant => format!("tenant:{tenant}"),
        RateScope::User => format!("user:{tenant}:{}", subject.unwrap_or("anonymous")),
        RateScope::Ip => format!("ip:{tenant}:{}", client_ip.unwrap_or("unknown")),
        RateScope::Route => format!("route:{}", route.unwrap_or(upstream)),
    }
}

/// Whether the strategy admits the request when the bucket is empty.
///
/// All three configured strategies resolve to rejection in this
/// configuration; the function exists so the resolution is explicit and
/// testable rather than implicit.
#[must_use]
pub const fn admits_on_empty(_strategy: RateStrategy) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use crate::domain::model::{Burst, RateAlgorithm, Sharing, Sustained, Window};

    fn limit(rate: u32, capacity: Option<u32>, cost: u32) -> RateLimit {
        RateLimit {
            sharing: Sharing::Private,
            algorithm: RateAlgorithm::TokenBucket,
            sustained: Sustained {
                rate,
                window: Window::Second,
            },
            burst: Burst { capacity },
            scope: RateScope::Tenant,
            strategy: RateStrategy::Reject,
            cost,
        }
    }

    #[test]
    fn a_full_bucket_admits_up_to_capacity_then_rejects() {
        let l = Limiter::new();
        let rl = limit(2, Some(2), 1);
        let t = Instant::now();
        assert!(l.check_at("k", &rl, t).allowed);
        assert!(l.check_at("k", &rl, t).allowed);
        let d = l.check_at("k", &rl, t);
        assert!(!d.allowed);
        assert!(d.retry_after_secs >= 1);
    }

    #[test]
    fn tokens_replenish_over_time() {
        let l = Limiter::new();
        let rl = limit(1, Some(1), 1);
        let t = Instant::now();
        assert!(l.check_at("k", &rl, t).allowed);
        assert!(!l.check_at("k", &rl, t).allowed);
        // One second later exactly one token is back.
        let t2 = t + Duration::from_secs(1);
        assert!(l.check_at("k", &rl, t2).allowed);
    }

    #[test]
    fn distinct_keys_do_not_share_a_bucket() {
        let l = Limiter::new();
        let rl = limit(1, Some(1), 1);
        let t = Instant::now();
        assert!(l.check_at("a", &rl, t).allowed);
        assert!(l.check_at("b", &rl, t).allowed);
    }

    #[test]
    fn cost_consumes_multiple_tokens() {
        let l = Limiter::new();
        let rl = limit(10, Some(10), 5);
        let t = Instant::now();
        assert!(l.check_at("k", &rl, t).allowed);
        assert!(l.check_at("k", &rl, t).allowed);
        assert!(!l.check_at("k", &rl, t).allowed);
    }

    #[test]
    fn headers_report_the_configured_ceiling_and_remaining() {
        let l = Limiter::new();
        let rl = limit(5, Some(5), 1);
        let d = l.check_at("k", &rl, Instant::now());
        assert_eq!(d.limit, 5);
        assert_eq!(d.remaining, 4);
    }

    #[test]
    fn a_minute_window_refills_proportionally() {
        let l = Limiter::new();
        let mut rl = limit(60, Some(1), 1);
        rl.sustained.window = Window::Minute;
        let t = Instant::now();
        assert!(l.check_at("k", &rl, t).allowed);
        assert!(!l.check_at("k", &rl, t).allowed);
        assert!(l.check_at("k", &rl, t + Duration::from_secs(1)).allowed);
    }

    #[test]
    fn queue_and_degrade_resolve_to_rejection() {
        assert!(!admits_on_empty(RateStrategy::Queue));
        assert!(!admits_on_empty(RateStrategy::Degrade));
        assert!(!admits_on_empty(RateStrategy::Reject));
    }

    #[test]
    fn scope_keys_are_distinct_per_scope() {
        let k = |s| scope_key(s, "t1", Some("s1"), Some("1.2.3.4"), Some("r1"), "u1");
        let all = [
            k(RateScope::Global),
            k(RateScope::Tenant),
            k(RateScope::User),
            k(RateScope::Ip),
            k(RateScope::Route),
        ];
        let mut uniq = all.to_vec();
        uniq.sort();
        uniq.dedup();
        assert_eq!(uniq.len(), all.len());
    }
}
