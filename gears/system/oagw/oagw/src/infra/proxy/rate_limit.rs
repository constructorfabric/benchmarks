//! The token-bucket rate limiter of the request-phase guard stage
//! (`cpt-cf-oagw-algo-rate-limit`, `cpt-cf-oagw-algo-bucket-consume`,
//! `cpt-cf-oagw-state-rate-limit-decision`).
//!
//! The limiter is the in-process bucket table ADR 0003 describes: one bucket
//! per scope key, refilled continuously, accounted in fractional tokens and
//! never persisted, so a restart resets every bucket
//! (`cpt-cf-oagw-dod-rate-limit`). The check is free of I/O, holds no lock
//! across an await and stays inside the sub-millisecond budget the ADR states
//! (`inst-abc-15`).
//!
//! The three strategies the configuration names are applied here: `reject`
//! answers `429` with the rate-limit headers, `queue` holds the request in a
//! bounded in-process wait and `degrade` admits the request without consuming
//! from the exhausted bucket (`inst-abc-07` to `inst-abc-14`).

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use parking_lot::Mutex;

use crate::domain::model::{RateLimitConfig, RateScope, RateStrategy, RateWindow};

/// The number of refill polls a queued request is held for at most.
///
/// The bound is the queue depth of ADR 0003: a request is never held without a
/// bound, and a bound exceeded produces the same `429` the `reject` strategy
/// produces (`inst-abc-11`, `inst-abc-12`, `inst-srl-04`).
pub const MAX_QUEUE_ATTEMPTS: u32 = 8;

/// The interval two consecutive refill polls of a queued request are apart.
pub const QUEUE_POLL: Duration = Duration::from_millis(25);

/// The wire token of the disposition a throttled request carries
/// (`inst-srl-05`).
pub const REJECTED_DECISION: &str = "rejected";

/// The disposition of one rate-limit decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateDecision {
    /// The bucket satisfied the cost: the tokens are consumed and the chain
    /// continues (`inst-srl-01`).
    Admitted,
    /// The bucket could not satisfy the cost and the strategy is `queue`: the
    /// request entered the bounded in-process wait (`inst-srl-02`).
    Queued,
    /// The bucket could not satisfy the cost and the strategy is `degrade`: the
    /// request proceeds without consuming tokens (`inst-srl-06`).
    Degraded,
    /// The request is refused with `429` (`inst-srl-05`).
    Rejected,
}

impl RateDecision {
    /// The wire token the request context records.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Admitted => "admitted",
            Self::Queued => "queued",
            Self::Degraded => "degraded",
            Self::Rejected => REJECTED_DECISION,
        }
    }
}

/// The rate-limit check a request is accounted by (`inst-arl-10`).
#[derive(Debug, Clone)]
pub struct RatePlan {
    /// The bucket key: the rate-limit configuration identity, the scope and the
    /// scope id (`inst-arl-08`).
    pub key: String,
    /// The effective limit and capacity of the bucket, in tokens.
    pub limit: u64,
    /// The refill rate, in tokens per second (`inst-arl-01`).
    pub refill: f64,
    /// The number of tokens the request consumes (`inst-arl-09`).
    pub cost: u64,
    /// The disposition of a request the bucket cannot satisfy.
    pub strategy: RateStrategy,
    /// Whether the `X-RateLimit-*` headers are emitted with a `429`.
    pub response_headers: bool,
}

/// The outcome of one rate-limit check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateOutcome {
    /// The disposition the request ended in.
    pub decision: RateDecision,
    /// The whole tokens left in the bucket after the check.
    pub remaining: u64,
    /// The whole seconds until the bucket can satisfy the cost.
    pub retry_after: Option<u32>,
    /// The Unix epoch second the bucket returns to full capacity.
    pub reset: Option<u64>,
}

/// One bucket of the process-local table.
#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    updated: std::time::Instant,
}

/// The process-local bucket table.
///
/// Buckets live in process memory only: nothing is synchronized across nodes,
/// nothing is read from or written to a `rate_limit_sync` configuration and a
/// restart discards the table with the decisions it made.
#[derive(Debug, Default)]
pub struct RateLimiter {
    buckets: DashMap<String, Mutex<Bucket>>,
}

impl RateLimiter {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// One account of the bucket table, for the observability layer.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buckets.len()
    }

    /// Whether the table holds no bucket.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }

    /// Attempt the acquisition of `plan.cost` from the bucket `plan` names.
    ///
    /// The check is synchronous, holds its lock for the arithmetic only and
    /// never awaits (`inst-abc-15`). A `queue` disposition is the intermediate
    /// state [`RateDecision::Queued`]; the caller turns it into a rejection
    /// when a bound is exceeded first.
    #[must_use]
    pub fn try_acquire(&self, plan: &RatePlan) -> RateOutcome {
        let capacity = f64::from(u32::try_from(plan.limit).unwrap_or(u32::MAX));
        // @cpt-begin:cpt-cf-oagw-algo-bucket-consume:p1:inst-abc-01
        // The bucket for the key is looked up in the process-local table and
        // created full at the effective capacity on first use.
        let entry = self
            .buckets
            .entry(plan.key.clone())
            .or_insert_with(|| Mutex::new(Bucket {
                tokens: capacity,
                updated: std::time::Instant::now(),
            }));
        let mut bucket = entry.lock();
        // @cpt-end:cpt-cf-oagw-algo-bucket-consume:p1:inst-abc-01

        // @cpt-begin:cpt-cf-oagw-algo-bucket-consume:p1:inst-abc-02
        // The refill adds the elapsed time since the last update multiplied by
        // the refill rate, capped at the capacity, and records the update time.
        refill(&mut bucket, plan, capacity);
        // @cpt-end:cpt-cf-oagw-algo-bucket-consume:p1:inst-abc-02

        let cost = f64::from(u32::try_from(plan.cost).unwrap_or(u32::MAX));
        // @cpt-begin:cpt-cf-oagw-state-rate-limit-decision:p1:inst-srl-01
        if bucket.tokens >= cost {
            // @cpt-begin:cpt-cf-oagw-algo-bucket-consume:p1:inst-abc-03
            // @cpt-begin:cpt-cf-oagw-algo-bucket-consume:p1:inst-abc-04
            // The cost is subtracted, the fractional remainder is kept and the
            // admission reports the remaining tokens floored to a whole number.
            bucket.tokens -= cost;
            let remaining = bucket.tokens.floor().max(0.0) as u64;
            // @cpt-end:cpt-cf-oagw-algo-bucket-consume:p1:inst-abc-04
            // @cpt-end:cpt-cf-oagw-algo-bucket-consume:p1:inst-abc-03
            return RateOutcome {
                decision: RateDecision::Admitted,
                remaining,
                retry_after: None,
                reset: None,
            };
            // @cpt-end:cpt-cf-oagw-state-rate-limit-decision:p1:inst-srl-01
        }

        // @cpt-begin:cpt-cf-oagw-algo-bucket-consume:p1:inst-abc-05
        // @cpt-begin:cpt-cf-oagw-algo-bucket-consume:p1:inst-abc-06
        // The shortfall is the time the bucket needs to satisfy the cost and
        // the time it needs to fill up again.
        let refillable = plan.refill.max(f64::MIN_POSITIVE);
        let to_cost = Duration::from_secs_f64((cost - bucket.tokens) / refillable);
        let to_full = Duration::from_secs_f64((capacity - bucket.tokens) / refillable);
        let retry_after =
            u32::try_from(to_cost.as_secs() + u64::from(to_cost.subsec_nanos() > 0))
                .unwrap_or(u32::MAX);
        // @cpt-end:cpt-cf-oagw-algo-bucket-consume:p1:inst-abc-06
        // @cpt-end:cpt-cf-oagw-algo-bucket-consume:p1:inst-abc-05

        let decision = match plan.strategy {
            // @cpt-begin:cpt-cf-oagw-algo-bucket-consume:p1:inst-abc-08
            // The `reject` strategy is the rejection with the `Retry-After` and
            // the rate-limit header values, which the response phase attaches
            // when the configuration asked for them (`inst-rl-15`, `inst-arp-08`).
            // @cpt-begin:cpt-cf-oagw-state-rate-limit-decision:p1:inst-srl-05
            RateStrategy::Reject => RateDecision::Rejected,
            // @cpt-end:cpt-cf-oagw-state-rate-limit-decision:p1:inst-srl-05
            // @cpt-end:cpt-cf-oagw-algo-bucket-consume:p1:inst-abc-08
            // @cpt-begin:cpt-cf-oagw-algo-bucket-consume:p1:inst-abc-09
            // @cpt-begin:cpt-cf-oagw-algo-bucket-consume:p1:inst-abc-10
            // The `queue` strategy is the intermediate state, which
            // [`RateLimiter::admit`] holds in the bounded in-process wait for
            // this bucket, bounded by the queue depth and by the request's
            // remaining budget, and wakes when the refill satisfies the cost.
            // @cpt-begin:cpt-cf-oagw-state-rate-limit-decision:p1:inst-srl-02
            RateStrategy::Queue => RateDecision::Queued,
            // @cpt-end:cpt-cf-oagw-state-rate-limit-decision:p1:inst-srl-02
            // @cpt-end:cpt-cf-oagw-algo-bucket-consume:p1:inst-abc-10
            // @cpt-end:cpt-cf-oagw-algo-bucket-consume:p1:inst-abc-09
            // @cpt-begin:cpt-cf-oagw-algo-bucket-consume:p1:inst-abc-13
            // @cpt-begin:cpt-cf-oagw-algo-bucket-consume:p1:inst-abc-14
            // A degraded request is admitted without consuming, so the bucket
            // stays untouched for the requests that can be satisfied.
            // @cpt-begin:cpt-cf-oagw-state-rate-limit-decision:p1:inst-srl-06
            RateStrategy::Degrade => RateDecision::Degraded,
            // @cpt-end:cpt-cf-oagw-state-rate-limit-decision:p1:inst-srl-06
            // @cpt-end:cpt-cf-oagw-algo-bucket-consume:p1:inst-abc-14
            // @cpt-end:cpt-cf-oagw-algo-bucket-consume:p1:inst-abc-13
        };
        // @cpt-begin:cpt-cf-oagw-algo-bucket-consume:p1:inst-abc-16
        // The disposition of the request is the outcome, with the shortfall
        // times the response phase turns into headers.
        RateOutcome {
            decision,
            remaining: 0,
            retry_after: Some(retry_after),
            reset: Some(epoch_after(to_full)),
        }
        // @cpt-end:cpt-cf-oagw-algo-bucket-consume:p1:inst-abc-16
    }

    /// Acquire `plan`, holding a `queue` disposition inside its bounds.
    ///
    /// The wait is bounded by the queue depth and by `budget`, the request's
    /// remaining deadline; whichever is exceeded first produces the same `429`
    /// the `reject` strategy produces (`inst-rl-07` to `inst-rl-11`).
    ///
    /// # Errors
    ///
    /// Never fails: the disposition of the request is the outcome.
    pub async fn admit(&self, plan: &RatePlan, budget: Duration) -> RateOutcome {
        let deadline = tokio::time::Instant::now() + budget;
        let mut attempts = 0_u32;
        let mut outcome = self.try_acquire(plan);
        while outcome.decision == RateDecision::Queued {
            attempts += 1;
            let now = tokio::time::Instant::now();
            if attempts > MAX_QUEUE_ATTEMPTS || now >= deadline {
                // @cpt-begin:cpt-cf-oagw-state-rate-limit-decision:p1:inst-srl-04
                // @cpt-begin:cpt-cf-oagw-algo-bucket-consume:p1:inst-abc-11
                // @cpt-begin:cpt-cf-oagw-algo-bucket-consume:p1:inst-abc-12
                // A bound exceeded first releases the request with the same
                // rejection the `reject` strategy produces.
                outcome.decision = RateDecision::Rejected;
                return outcome;
                // @cpt-end:cpt-cf-oagw-algo-bucket-consume:p1:inst-abc-12
                // @cpt-end:cpt-cf-oagw-algo-bucket-consume:p1:inst-abc-11
                // @cpt-end:cpt-cf-oagw-state-rate-limit-decision:p1:inst-srl-04
            }
            tokio::time::sleep(QUEUE_POLL.min(deadline - now)).await;
            // @cpt-begin:cpt-cf-oagw-state-rate-limit-decision:p1:inst-srl-03
            // A refill that satisfies the cost inside both the queue-depth bound
            // and the request's remaining budget turns the queued decision into
            // an admission, which continues the chain.
            outcome = self.try_acquire(plan);
            // @cpt-end:cpt-cf-oagw-state-rate-limit-decision:p1:inst-srl-03
        }
        outcome
    }
}

/// Refill `bucket` for the time elapsed since its last update.
fn refill(bucket: &mut Bucket, plan: &RatePlan, capacity: f64) {
    let now = std::time::Instant::now();
    let elapsed = now.saturating_duration_since(bucket.updated);
    bucket.updated = now;
    let gained = elapsed.as_secs_f64() * plan.refill;
    bucket.tokens = (bucket.tokens + gained).min(capacity);
}

/// The Unix epoch second `delay` from now.
fn epoch_after(delay: Duration) -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .saturating_add(delay)
        .as_secs()
}

/// The scope id of one request, with the fallback the unavailable identity
/// component takes (`inst-arl-05` to `inst-arl-07`).
///
/// Returns the resolved scope and the id it keys on. A request with no peer
/// address for the `ip` scope falls back to the `tenant` scope key, so a
/// request is never left unaccounted.
#[must_use]
pub fn scope_of(
    scope: RateScope,
    tenant: &str,
    subject: &str,
    peer_ip: Option<&str>,
    route: &str,
) -> (RateScope, String) {
    let (scope, id, _fallback) = scope_with_fallback(scope, tenant, subject, peer_ip, route);
    (scope, id)
}

/// The scope id of one request with the fallback flag the request context
/// records (`inst-arl-05` to `inst-arl-07`).
///
/// The third member is whether the configured scope's identity was unavailable
/// and the request was accounted under the `tenant` scope key instead.
#[must_use]
pub fn scope_with_fallback(
    scope: RateScope,
    tenant: &str,
    subject: &str,
    peer_ip: Option<&str>,
    route: &str,
) -> (RateScope, String, bool) {
    match scope {
        RateScope::Global => (RateScope::Global, "global".to_owned(), false),
        RateScope::Tenant => (RateScope::Tenant, tenant.to_owned(), false),
        RateScope::User => (RateScope::User, subject.to_owned(), false),
        // @cpt-begin:cpt-cf-oagw-algo-rate-limit:p1:inst-arl-06
        // @cpt-begin:cpt-cf-oagw-algo-rate-limit:p1:inst-arl-07
        // An unavailable identity component falls back to the tenant scope
        // key, which the caller records on the request context.
        RateScope::Ip => peer_ip.map_or_else(
            || (RateScope::Tenant, tenant.to_owned(), true),
            |peer_ip| (RateScope::Ip, peer_ip.to_owned(), false),
        ),
        // @cpt-end:cpt-cf-oagw-algo-rate-limit:p1:inst-arl-07
        // @cpt-end:cpt-cf-oagw-algo-rate-limit:p1:inst-arl-06
        RateScope::Route => (RateScope::Route, route.to_owned(), false),
    }
}

/// The bucket key of one request (`inst-arl-08`).
///
/// The rate-limit configuration identity is the upstream the request resolved
/// to, so two upstreams with equal limits do not share a bucket.
#[must_use]
pub fn bucket_key(upstream_id: &str, scope: RateScope, scope_id: &str) -> String {
    format!("rate_limit:{upstream_id}:{}:{scope_id}", scope.as_str())
}

/// The rate-limit check of one request, from the effective configuration and
/// the request facts (`inst-arl-10`).
///
/// The effective limit is the one the merged configuration carries, which is
/// already `min(ancestor.enforced, descendant)` over the enforced-ancestor set
/// (`inst-arl-03`, `inst-arl-04`): the merge applies the cap, and a descendant
/// cannot raise an enforced ancestor's limit.
#[must_use]
pub fn plan_of(
    config: &RateLimitConfig,
    upstream_id: &str,
    route: &str,
    tenant: &str,
    subject: &str,
    peer_ip: Option<&str>,
) -> RatePlan {
    plan_and_scope(config, upstream_id, route, tenant, subject, peer_ip).0
}

/// The rate-limit check of one request with the scope facts the request context
/// records (`inst-arl-06`, `inst-arl-07`).
///
/// The second member is the scope the bucket was keyed on and the third whether
/// the configured scope's identity was unavailable, which sends the request
/// under the `tenant` scope key instead.
#[must_use]
pub fn plan_and_scope(
    config: &RateLimitConfig,
    upstream_id: &str,
    route: &str,
    tenant: &str,
    subject: &str,
    peer_ip: Option<&str>,
) -> (RatePlan, RateScope, bool) {
    // @cpt-begin:cpt-cf-oagw-algo-rate-limit:p1:inst-arl-05
    // @cpt-begin:cpt-cf-oagw-algo-rate-limit:p1:inst-arl-06
    // @cpt-begin:cpt-cf-oagw-algo-rate-limit:p1:inst-arl-07
    let (scope, scope_id, scope_fallback) = scope_with_fallback(
        config.scope,
        tenant,
        subject,
        peer_ip,
        route,
    );
    // @cpt-end:cpt-cf-oagw-algo-rate-limit:p1:inst-arl-07
    // @cpt-end:cpt-cf-oagw-algo-rate-limit:p1:inst-arl-06
    // @cpt-end:cpt-cf-oagw-algo-rate-limit:p1:inst-arl-05
    (
        RatePlan {
            // @cpt-begin:cpt-cf-oagw-algo-rate-limit:p1:inst-arl-08
            key: bucket_key(upstream_id, scope, &scope_id),
            // @cpt-end:cpt-cf-oagw-algo-rate-limit:p1:inst-arl-08
            // @cpt-begin:cpt-cf-oagw-algo-rate-limit:p1:inst-arl-03
            // @cpt-begin:cpt-cf-oagw-algo-rate-limit:p1:inst-arl-04
            // The effective limit is the merged one: `min(ancestor.enforced,
            // descendant)`, with a `private` ancestor left out and an
            // `inherit` ancestor's limit taken from its parent.
            limit: config.effective_capacity(),
            // @cpt-end:cpt-cf-oagw-algo-rate-limit:p1:inst-arl-04
            // @cpt-end:cpt-cf-oagw-algo-rate-limit:p1:inst-arl-03
            // @cpt-begin:cpt-cf-oagw-algo-rate-limit:p1:inst-arl-01
            refill: refill_rate(config.sustained.rate, config.sustained.window),
            // @cpt-end:cpt-cf-oagw-algo-rate-limit:p1:inst-arl-01
            // @cpt-begin:cpt-cf-oagw-algo-rate-limit:p1:inst-arl-09
            cost: config.cost.max(1),
            // @cpt-end:cpt-cf-oagw-algo-rate-limit:p1:inst-arl-09
            strategy: config.strategy,
            // The schema has no `response_headers` member, so the recorded
            // default of ADR 0003 applies and the headers are always emitted.
            response_headers: true,
        },
        scope,
        scope_fallback,
    )
}

/// The refill rate of a sustained rate, in tokens per second (`inst-arl-01`).
#[must_use]
pub fn refill_rate(rate: u64, window: RateWindow) -> f64 {
    let seconds = window.seconds();
    let window = f64::from(u32::try_from(seconds).unwrap_or(u32::MAX));
    f64::from(u32::try_from(rate).unwrap_or(u32::MAX)) / window.max(1.0)
}

impl fmt::Display for RateOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.decision.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{BurstRate, SustainedRate};

    fn config(rate: u64, window: RateWindow, capacity: Option<u64>) -> RateLimitConfig {
        RateLimitConfig {
            sharing: crate::domain::model::Sharing::Inherit,
            algorithm: crate::domain::model::RateAlgorithm::TokenBucket,
            sustained: SustainedRate { rate, window },
            burst: Some(BurstRate { capacity }),
            scope: RateScope::Tenant,
            strategy: RateStrategy::Reject,
            cost: 1,
        }
    }

    fn plan(config: &RateLimitConfig, key: &str) -> RatePlan {
        RatePlan {
            key: key.to_owned(),
            limit: config.effective_capacity(),
            refill: refill_rate(config.sustained.rate, config.sustained.window),
            cost: config.cost,
            strategy: config.strategy,
            response_headers: true,
        }
    }

    #[test]
    fn the_refill_rate_is_the_rate_over_the_window() {
        assert!((refill_rate(10, RateWindow::Second) - 10.0).abs() < f64::EPSILON);
        assert!((refill_rate(60, RateWindow::Minute) - 1.0).abs() < f64::EPSILON);
        assert!((refill_rate(8_640, RateWindow::Day) - 0.1).abs() < 0.000_001);
        assert!((refill_rate(3_600, RateWindow::Hour) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn the_capacity_defaults_to_the_sustained_rate() {
        assert_eq!(config(50, RateWindow::Second, None).effective_capacity(), 50);
        assert_eq!(
            config(50, RateWindow::Second, Some(120)).effective_capacity(),
            120
        );
    }

    #[test]
    fn a_request_consumes_its_cost_and_reports_the_remaining() {
        let limiter = RateLimiter::new();
        let mut config = config(10, RateWindow::Second, None);
        config.cost = 2;
        let outcome = limiter.try_acquire(&plan(&config, "k"));
        assert_eq!(outcome.decision, RateDecision::Admitted);
        assert_eq!(outcome.remaining, 8, "the cost is consumed, the floor is kept");
        assert!(outcome.retry_after.is_none());
        assert!(outcome.reset.is_none());
    }

    #[test]
    fn a_burst_up_to_the_capacity_is_admitted() {
        let limiter = RateLimiter::new();
        let config = config(1, RateWindow::Minute, Some(3));
        let key = "burst";
        for expected in [2_u64, 1, 0] {
            let outcome = limiter.try_acquire(&plan(&config, key));
            assert_eq!(outcome.decision, RateDecision::Admitted);
            assert_eq!(outcome.remaining, expected, "the bucket starts full");
        }
        let outcome = limiter.try_acquire(&plan(&config, key));
        assert_eq!(outcome.decision, RateDecision::Rejected);
        assert_eq!(outcome.remaining, 0);
    }

    #[test]
    fn a_rejected_request_carries_the_retry_and_reset_values() {
        let limiter = RateLimiter::new();
        let config = config(60, RateWindow::Minute, Some(1));
        let key = "reject";
        assert_eq!(limiter.try_acquire(&plan(&config, key)).decision, RateDecision::Admitted);
        let outcome = limiter.try_acquire(&plan(&config, key));
        assert_eq!(outcome.decision, RateDecision::Rejected);
        assert_eq!(outcome.retry_after, Some(1), "one token at 1/s costs 1s");
        let reset = outcome.reset.expect("the reset is the second the bucket fills");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_secs();
        assert!(
            reset >= now && reset <= now + 2,
            "the reset is the epoch second the bucket returns to full"
        );
    }

    #[tokio::test]
    async fn a_queued_request_is_admitted_once_the_refill_satisfies_its_cost() {
        let limiter = RateLimiter::new();
        let mut config = config(50, RateWindow::Second, Some(1));
        config.strategy = RateStrategy::Queue;
        let key = "queue";
        assert_eq!(
            limiter.try_acquire(&plan(&config, key)).decision,
            RateDecision::Admitted,
            "the first request drains the bucket"
        );
        assert_eq!(limiter.try_acquire(&plan(&config, key)).decision, RateDecision::Queued);
        let outcome = limiter.admit(&plan(&config, key), Duration::from_secs(5)).await;
        assert_eq!(outcome.decision, RateDecision::Admitted, "the refill satisfies the cost");
    }

    #[tokio::test]
    async fn a_queue_bound_exceeded_first_produces_the_reject_outcome() {
        let limiter = RateLimiter::new();
        let mut config = config(1, RateWindow::Day, None);
        config.strategy = RateStrategy::Queue;
        config.cost = 5;
        let key = "queue-bound";
        let outcome = limiter.admit(&plan(&config, key), Duration::from_millis(10)).await;
        assert_eq!(outcome.decision, RateDecision::Rejected);
        assert!(outcome.retry_after.is_some());
    }

    #[test]
    fn a_degraded_request_consumes_nothing() {
        let limiter = RateLimiter::new();
        let mut config = config(1, RateWindow::Day, None);
        config.strategy = RateStrategy::Degrade;
        let key = "degrade";
        assert_eq!(limiter.try_acquire(&plan(&config, key)).decision, RateDecision::Admitted);
        for _ in 0..5 {
            let outcome = limiter.try_acquire(&plan(&config, key));
            assert_eq!(outcome.decision, RateDecision::Degraded);
            assert_eq!(outcome.remaining, 0);
        }
        // The bucket is untouched: a request that can be satisfied is still
        // served from it.
        let mut restored = config;
        restored.strategy = RateStrategy::Reject;
        assert_eq!(
            limiter.try_acquire(&plan(&restored, key)).decision,
            RateDecision::Rejected,
            "the degraded requests consumed nothing, so the bucket is still empty"
        );
    }

    #[test]
    fn each_scope_produces_its_own_bucket_key() {
        let tenant = bucket_key("u-1", RateScope::Tenant, "t-1");
        assert_eq!(tenant, "rate_limit:u-1:tenant:t-1");
        assert_ne!(tenant, bucket_key("u-2", RateScope::Tenant, "t-1"), "two upstreams do not share a bucket");
        assert_ne!(tenant, bucket_key("u-1", RateScope::User, "t-1"));
        assert_ne!(tenant, bucket_key("u-1", RateScope::Global, "global"));
        assert_ne!(tenant, bucket_key("u-1", RateScope::Route, "u-1//v1"));
        assert_ne!(
            bucket_key("u-1", RateScope::Ip, "10.0.0.1"),
            bucket_key("u-1", RateScope::Ip, "10.0.0.2"),
            "each peer address keys its own bucket"
        );
    }

    #[test]
    fn an_unavailable_ip_scope_falls_back_to_the_tenant_scope() {
        let (scope, id) = scope_of(RateScope::Ip, "t-1", "s-1", None, "/v1");
        assert_eq!(scope, RateScope::Tenant, "the fallback is the tenant scope");
        assert_eq!(id, "t-1");
        let (scope, id) = scope_of(RateScope::Ip, "t-1", "s-1", Some("10.0.0.9"), "/v1");
        assert_eq!(scope, RateScope::Ip);
        assert_eq!(id, "10.0.0.9");
    }

    #[test]
    fn the_scope_ids_follow_the_documented_keys() {
        assert_eq!(
            scope_of(RateScope::Global, "t", "s", None, "/v1").1,
            "global",
            "one bucket per process"
        );
        assert_eq!(scope_of(RateScope::Tenant, "t-1", "s-1", None, "/v1").1, "t-1");
        assert_eq!(scope_of(RateScope::User, "t-1", "s-1", None, "/v1").1, "s-1");
        assert_eq!(
            scope_of(RateScope::Route, "t-1", "s-1", None, "u-1//v1").1,
            "u-1//v1",
            "the matched upstream and route identity"
        );
    }

    #[test]
    fn the_plan_of_a_config_carries_the_bucket_facts() {
        let config = config(120, RateWindow::Minute, Some(200));
        let plan = plan_of(&config, "u-1", "/v1", "t-1", "s-1", None);
        assert_eq!(plan.key, "rate_limit:u-1:tenant:t-1");
        assert_eq!(plan.limit, 200);
        assert!((plan.refill - 2.0).abs() < f64::EPSILON);
        assert_eq!(plan.cost, 1);
        assert_eq!(plan.strategy, RateStrategy::Reject);
        assert!(plan.response_headers);
    }

    #[test]
    fn the_decision_carries_its_wire_token() {
        assert_eq!(RateDecision::Admitted.as_str(), "admitted");
        assert_eq!(RateDecision::Queued.as_str(), "queued");
        assert_eq!(RateDecision::Degraded.as_str(), "degraded");
        assert_eq!(RateDecision::Rejected.as_str(), "rejected");
    }

    #[test]
    fn the_table_reports_its_buckets() {
        let limiter = RateLimiter::new();
        assert!(limiter.is_empty());
        let config = config(1, RateWindow::Second, None);
        let _ = limiter.try_acquire(&plan(&config, "k"));
        assert_eq!(limiter.len(), 1);
        assert!(!limiter.is_empty());
    }
}
