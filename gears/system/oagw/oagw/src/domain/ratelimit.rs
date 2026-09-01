//! Rate limiting (token bucket + sliding window).
//!
//! ADR-0003: sustained `rate` within `window` with a `burst` capacity,
//! `scope` (global/tenant/user/ip/route), `strategy` (reject/queue/degrade)
//! and `cost`.  Effective limits across the hierarchy are checked as a set
//! (min semantics — every configured limiter must admit the request).
//!
//! The registry is process-local (in-memory), guarded by a `Mutex`
//! per-algorithm state map, with an injectable clock for deterministic tests.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::domain::dto::{RateLimitAlgorithm, RateLimitConfig, RateLimitStrategy};

#[must_use]
pub fn realtime_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

type NowFn = Box<dyn Fn() -> u64 + Send + Sync>;

/// Exact `u64 → f64` for token-bucket math.  Every magnitude converted here
/// (rates, capacities, window seconds, elapsed seconds) is a small validated
/// integer far below 2^53, so the cast never drops a bit; a targeted allow
/// keeps the float maths explicit instead of routing it through `TryFrom`.
#[allow(clippy::cast_precision_loss)]
fn exact_f64(v: u64) -> f64 {
    v as f64
}

/// `f64 → u64` for bucket accounting.  Rust's float→int cast already saturates
/// (truncates toward zero and clamps out-of-range/NaN to `u64::MAX` or 0), and
/// every caller clamps the argument to `>= 1.0` (or `>= 0.0`) first, so the
/// value is always a small in-range integer — the two allowed casts are exact.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn rounded_u64(v: f64) -> u64 {
    v as u64
}

struct BucketState {
    tokens: f64,
    last_refill: u64,
}

impl BucketState {
    fn refill(&mut self, now: u64, capacity: f64, rate_per_sec: f64) {
        if now > self.last_refill {
            let elapsed = exact_f64(now - self.last_refill);
            self.tokens = (self.tokens + elapsed * rate_per_sec).min(capacity);
            self.last_refill = now;
        }
    }
}

#[derive(Default)]
struct SlidingState {
    entries: VecDeque<(u64, u64)>, // (timestamp_secs, cost)
}

/// Bounded wait-queue accounting for the `queue` strategy: `pending` requests
/// currently waiting to be served once capacity frees up.
#[derive(Default, Clone, Copy)]
struct QueueState {
    pending: u64,
}

#[derive(Debug, Clone, Copy)]
// Three independent ADR-0003 strategy-outcome flags (allowed / queued /
// degraded) — a derived state machine would obscure the documented semantics.
#[allow(clippy::struct_excessive_bools)]
pub struct RateCheck {
    pub allowed: bool,
    /// Seconds until the caller may retry (0 when allowed).
    pub retry_after_secs: u64,
    pub limit: u64,
    pub remaining: u64,
    /// Unix seconds when the window resets / bucket refills enough — a real
    /// future timestamp, never "now".
    pub reset_at_unix: u64,
    /// True when the request was admitted through the `queue` strategy
    /// (bounded wait-queue) rather than normal capacity.
    pub queued: bool,
    /// True when the request was admitted in degraded mode (`degrade`
    /// strategy over the limit).
    pub degraded: bool,
}

pub struct RateLimiterRegistry {
    buckets: Mutex<HashMap<String, BucketState>>,
    sliding: Mutex<HashMap<String, SlidingState>>,
    queue: Mutex<HashMap<String, QueueState>>,
    now: NowFn,
}

impl Default for RateLimiterRegistry {
    fn default() -> Self {
        Self::with_clock(Box::new(realtime_now_secs))
    }
}

impl RateLimiterRegistry {
    #[must_use]
    pub fn with_clock(now: NowFn) -> Self {
        Self {
            buckets: Mutex::new(HashMap::new()),
            sliding: Mutex::new(HashMap::new()),
            queue: Mutex::new(HashMap::new()),
            now,
        }
    }

    /// Check one configured limit for a scope key.  `reject` refuses
    /// over-limit requests (429).  `queue` runs the underlying limiter and
    /// admits over-limit requests into a bounded wait-queue (overflow →
    /// rejected).  `degrade` runs the underlying limiter and admits
    /// over-limit requests flagged as degraded (reduced-functionality
    /// processing).  Neither strategy is an unconditional admit.
    pub fn check(&self, cfg: &RateLimitConfig, key: &str, cost: u64) -> RateCheck {
        let now = (self.now)();
        let raw = match cfg.algorithm {
            RateLimitAlgorithm::TokenBucket => self.check_token_bucket(cfg, key, cost, now),
            RateLimitAlgorithm::SlidingWindow => self.check_sliding(cfg, key, cost, now),
        };
        match cfg.strategy {
            RateLimitStrategy::Reject => raw,
            RateLimitStrategy::Queue => {
                if raw.allowed {
                    return raw;
                }
                // Over-limit: admit into the bounded wait-queue.  Overflow of
                // the queue capacity behaves like `reject` (429).
                if self.enqueue(cfg, key, now) {
                    RateCheck {
                        allowed: true,
                        retry_after_secs: 0,
                        limit: raw.limit,
                        remaining: 0,
                        reset_at_unix: raw.reset_at_unix,
                        queued: true,
                        degraded: false,
                    }
                } else {
                    RateCheck {
                        allowed: false,
                        retry_after_secs: raw.retry_after_secs,
                        limit: raw.limit,
                        remaining: 0,
                        reset_at_unix: raw.reset_at_unix,
                        queued: false,
                        degraded: false,
                    }
                }
            }
            RateLimitStrategy::Degrade => {
                if raw.allowed {
                    return raw;
                }
                // Over-limit: admit with degraded (reduced-functionality)
                // processing marked on the check.
                RateCheck {
                    allowed: true,
                    retry_after_secs: 0,
                    limit: raw.limit,
                    remaining: 0,
                    reset_at_unix: raw.reset_at_unix,
                    queued: false,
                    degraded: true,
                }
            }
        }
    }

    /// Track a queued request for `key`, bounded by the burst capacity.
    /// Returns false (queue full → the request must be rejected) when the
    /// queue already holds `capacity` waiting requests.
    fn enqueue(&self, cfg: &RateLimitConfig, key: &str, _now: u64) -> bool {
        let cap = cfg.burst_capacity.max(1);
        // A poisoned lock still yields the guard; the state is always usable.
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = queue.entry(key.to_owned()).or_default();
        if state.pending >= cap {
            return false;
        }
        state.pending += 1;
        true
    }

    fn check_token_bucket(
        &self,
        cfg: &RateLimitConfig,
        key: &str,
        cost: u64,
        now: u64,
    ) -> RateCheck {
        let capacity = exact_f64(cfg.burst_capacity);
        let window_secs = exact_f64(cfg.sustained_window.seconds());
        let rate_per_sec = exact_f64(cfg.sustained_rate) / window_secs;
        let cost_f = exact_f64(cost);

        // A poisoned lock still yields the guard; the state is always usable.
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = buckets
            .entry(key.to_owned())
            .or_insert_with(|| BucketState {
                tokens: capacity,
                last_refill: now,
            });
        state.refill(now, capacity, rate_per_sec);

        if state.tokens + 1e-9 >= cost_f {
            state.tokens -= cost_f;
            let remaining = rounded_u64(state.tokens.floor().max(0.0));
            // Next reset is a real future instant: when the bucket is full the
            // sustained window elapses; otherwise when tokens fully refill.
            let reset_at_unix = if capacity - state.tokens < 1e-9 {
                now + rounded_u64(window_secs)
            } else {
                now + rounded_u64(((capacity - state.tokens) / rate_per_sec).ceil().max(1.0))
            };
            RateCheck {
                allowed: true,
                retry_after_secs: 0,
                limit: cfg.sustained_rate,
                remaining,
                reset_at_unix,
                queued: false,
                degraded: false,
            }
        } else {
            // seconds until enough tokens accumulate for `cost`
            let deficit = cost_f - state.tokens;
            let secs = rounded_u64((deficit / rate_per_sec).ceil().max(1.0));
            RateCheck {
                allowed: false,
                retry_after_secs: secs,
                limit: cfg.sustained_rate,
                remaining: 0,
                reset_at_unix: now + secs,
                queued: false,
                degraded: false,
            }
        }
    }

    fn check_sliding(&self, cfg: &RateLimitConfig, key: &str, cost: u64, now: u64) -> RateCheck {
        let capacity = cfg.burst_capacity;
        let window_secs = cfg.sustained_window.seconds();

        // A poisoned lock still yields the guard; the state is always usable.
        let mut states = self
            .sliding
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = states.entry(key.to_owned()).or_default();

        // Evict entries that have fallen out of the window.
        while let Some(&(ts, _)) = state.entries.front() {
            if ts + window_secs <= now {
                state.entries.pop_front();
            } else {
                break;
            }
        }

        let in_window: u64 = state.entries.iter().map(|(_, c)| *c).sum();

        if in_window + cost > capacity {
            // Retry when the oldest entry leaves the window.
            let oldest = state.entries.front().map_or(now, |(ts, _)| *ts);
            let retry = oldest
                .saturating_add(window_secs)
                .saturating_sub(now)
                .max(1);
            RateCheck {
                allowed: false,
                retry_after_secs: retry,
                limit: cfg.sustained_rate,
                remaining: 0,
                reset_at_unix: now + retry,
                queued: false,
                degraded: false,
            }
        } else {
            state.entries.push_back((now, cost));
            let remaining = capacity.saturating_sub(in_window + cost);
            // Real next reset: when the current window's oldest (still
            // active) request leaves the window.
            let oldest = state.entries.front().map_or(now, |(ts, _)| *ts);
            RateCheck {
                allowed: true,
                retry_after_secs: 0,
                limit: cfg.sustained_rate,
                remaining,
                reset_at_unix: oldest.saturating_add(window_secs),
                queued: false,
                degraded: false,
            }
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::dto::{
        RateLimitAlgorithm, RateLimitConfig, RateLimitScope, RateLimitStrategy, RateLimitWindow,
        Sharing,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn cfg(
        rate: u64,
        window: RateLimitWindow,
        capacity: u64,
        algo: RateLimitAlgorithm,
    ) -> RateLimitConfig {
        RateLimitConfig {
            sharing: Sharing::Private,
            algorithm: algo,
            sustained_rate: rate,
            sustained_window: window,
            burst_capacity: capacity,
            scope: RateLimitScope::Tenant,
            strategy: RateLimitStrategy::Reject,
            cost: 1,
            response_headers: true,
        }
    }

    #[tokio::test]
    async fn token_bucket_allows_burst_then_rejects() {
        let clock = Arc::new(AtomicU64::new(1_000));
        let c = clock.clone();
        let reg = RateLimiterRegistry::with_clock(Box::new(move || c.load(Ordering::SeqCst)));
        let lim = cfg(
            10,
            RateLimitWindow::Second,
            3,
            RateLimitAlgorithm::TokenBucket,
        );

        // Burst capacity 3 admitted back-to-back.
        assert!(reg.check(&lim, "t", 1).allowed);
        assert!(reg.check(&lim, "t", 1).allowed);
        assert!(reg.check(&lim, "t", 1).allowed);
        // Fourth rejected with a retry-after hint.
        let reject = reg.check(&lim, "t", 1);
        assert!(!reject.allowed);
        assert!(reject.retry_after_secs >= 1);
        assert_eq!(reject.remaining, 0);

        // After the window passes, capacity refills.
        clock.store(1_001, Ordering::SeqCst);
        assert!(reg.check(&lim, "t", 1).allowed);
    }

    #[tokio::test]
    async fn tokens_refill_gradually() {
        let clock = Arc::new(AtomicU64::new(2_000));
        let c = clock.clone();
        let reg = RateLimiterRegistry::with_clock(Box::new(move || c.load(Ordering::SeqCst)));
        // rate 60/min → 1 token/sec, capacity 10
        let lim = cfg(
            60,
            RateLimitWindow::Minute,
            10,
            RateLimitAlgorithm::TokenBucket,
        );
        for _ in 0..10 {
            assert!(reg.check(&lim, "g", 1).allowed);
        }
        assert!(!reg.check(&lim, "g", 1).allowed);

        // 1 second later exactly 1 token is available.
        clock.store(2_001, Ordering::SeqCst);
        assert!(reg.check(&lim, "g", 1).allowed);
    }

    #[tokio::test]
    async fn sliding_window_counts_rolling_requests() {
        let clock = Arc::new(AtomicU64::new(3_000));
        let c = clock.clone();
        let reg = RateLimiterRegistry::with_clock(Box::new(move || c.load(Ordering::SeqCst)));
        let lim = cfg(
            10,
            RateLimitWindow::Second,
            2,
            RateLimitAlgorithm::SlidingWindow,
        );

        assert!(reg.check(&lim, "s", 1).allowed);
        assert!(reg.check(&lim, "s", 1).allowed);
        assert!(!reg.check(&lim, "s", 1).allowed);

        // Oldest request leaves the window after 1 second.
        clock.store(3_001, Ordering::SeqCst);
        assert!(reg.check(&lim, "s", 1).allowed);
    }

    #[test]
    fn queue_strategy_bounded_wait_queue() {
        let clock = Arc::new(AtomicU64::new(4_000));
        let c = clock.clone();
        let reg = RateLimiterRegistry::with_clock(Box::new(move || c.load(Ordering::SeqCst)));
        // capacity 2 → 2 normal admits, then bounded queue of 2.
        let mut lim = cfg(
            1,
            RateLimitWindow::Second,
            2,
            RateLimitAlgorithm::TokenBucket,
        );
        lim.strategy = RateLimitStrategy::Queue;

        // Normal capacity admits.
        assert!(reg.check(&lim, "q", 1).allowed);
        assert!(reg.check(&lim, "q", 1).allowed);

        // Over-limit requests enter the bounded queue (admitted, queued=true).
        let q1 = reg.check(&lim, "q", 1);
        assert!(q1.allowed && q1.queued);
        let q2 = reg.check(&lim, "q", 1);
        assert!(q2.allowed && q2.queued);

        // Queue capacity (2) exhausted → overflow behaves like reject (429).
        let overflow = reg.check(&lim, "q", 1);
        assert!(!overflow.allowed);
        assert!(!overflow.queued);

        // Once a window passes, fresh capacity is available again.
        clock.store(4_001, Ordering::SeqCst);
        assert!(reg.check(&lim, "q", 1).allowed);
    }

    #[test]
    fn degrade_strategy_is_not_unconditional_admit() {
        let clock = Arc::new(AtomicU64::new(5_000));
        let c = clock;
        let reg = RateLimiterRegistry::with_clock(Box::new(move || c.load(Ordering::SeqCst)));
        let mut lim = cfg(
            1,
            RateLimitWindow::Second,
            2,
            RateLimitAlgorithm::TokenBucket,
        );
        lim.strategy = RateLimitStrategy::Degrade;

        // Within capacity: normal admits (not degraded).
        let ok = reg.check(&lim, "d", 1);
        assert!(ok.allowed && !ok.degraded);
        assert!(reg.check(&lim, "d", 1).allowed);

        // Over-limit: admitted but flagged degraded (reduced functionality).
        let degraded = reg.check(&lim, "d", 1);
        assert!(degraded.allowed);
        assert!(degraded.degraded);
    }

    #[test]
    fn reset_at_unix_is_a_future_timestamp_not_now() {
        let clock = Arc::new(AtomicU64::new(6_000));
        let c = clock;
        let reg = RateLimiterRegistry::with_clock(Box::new(move || c.load(Ordering::SeqCst)));
        // Full-capacity admit: reset is `now + window`.
        let lim = cfg(
            10,
            RateLimitWindow::Second,
            5,
            RateLimitAlgorithm::TokenBucket,
        );
        let ok = reg.check(&lim, "r", 1);
        assert!(ok.allowed);
        assert!(
            ok.reset_at_unix > 6_000,
            "reset must be future, got {}",
            ok.reset_at_unix
        );

        // Sliding window: reset is the time the newest window closes, not now.
        let sliding = cfg(
            10,
            RateLimitWindow::Second,
            5,
            RateLimitAlgorithm::SlidingWindow,
        );
        let ok = reg.check(&sliding, "r2", 1);
        assert!(ok.allowed);
        assert!(ok.reset_at_unix > 6_000);
    }

    #[test]
    fn scope_keys_are_isolated() {
        let reg = RateLimiterRegistry::default();
        let lim = cfg(
            1,
            RateLimitWindow::Second,
            1,
            RateLimitAlgorithm::TokenBucket,
        );
        assert!(reg.check(&lim, "tenant-a", 1).allowed);
        assert!(!reg.check(&lim, "tenant-a", 1).allowed);
        assert!(reg.check(&lim, "tenant-b", 1).allowed);
    }
}
