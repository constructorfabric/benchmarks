//! The rate-limit check the proxy path is held to, its over-limit strategies,
//! the header set a refusal carries, and the cleanup a deletion notifies.
//!
//! Realizes `cpt-cf-oagw-flow-rate-limit-check`,
//! `cpt-cf-oagw-flow-rate-limit-strategy`, `cpt-cf-oagw-flow-rate-limit-cleanup`,
//! and `cpt-cf-oagw-algo-rate-limit-headers` of
//! `cpt-cf-oagw-feature-rate-limiting`, whose algorithms and registry live in
//! [`crate::domain::ratelimit`]. The module holds no transport type: the
//! handler assembles the identity the check keys on from the security context
//! and the connection, and maps [`LimitVerdict`] to the 429, the 503, or the
//! forward the proxy flow answers with.
//!
//! The check runs at the position the proxy flow states, ahead of the composed
//! plugin chain and after the body validation, and it runs once per admission
//! attempt, the queue's releases being the re-runs §2 records.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use uuid::Uuid;

use crate::domain::proxy::{MatchedRoute, ResolvedUpstream};
use crate::domain::ratelimit::{
    AcquireOutcome, EffectiveLimit, LimitLayer, LimitLayers, QUEUE_CAPACITY, QUEUE_WAIT,
    RateLimiterRegistry, fold, sliding_window, token_bucket, token_bucket_capped, window_millis,
};
use crate::domain::upstream::{Algorithm, RateLimitScope, Strategy};

/// The registry one gear holds, shared by the proxy path and the deletion
/// observer, which the check locks for the length of one acquisition and which
/// the queue's wait does not hold.
#[derive(Debug, Default, Clone)]
pub struct SharedLimits(Arc<Mutex<RateLimiterRegistry>>);

impl SharedLimits {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The registry one acquisition runs against.
    pub fn lock(&self) -> parking_lot::MutexGuard<'_, RateLimiterRegistry> {
        self.0.lock()
    }
}

/// The interval one queued request waits before it asks the counter again,
/// which spaces the re-runs the `queue` strategy performs inside its wait
/// bound. It is a build-time constant of this feature with no configuration
/// surface (§1.5).
const QUEUE_POLL: Duration = Duration::from_millis(20);

/// The `response_headers` gate this run holds, which is the declared default
/// ADR 0003's field table gives and which no written configuration can change,
/// because the shipped `definitions.rate_limit` declares
/// `additionalProperties: false` and lists no such member (§1.5).
pub const RESPONSE_HEADERS_GATE: bool = true;

/// The identifiers the counter key is formed from, which the proxy path
/// gathers from the security context and the inbound connection.
#[derive(Debug, Clone)]
pub struct LimitIdentity {
    /// The calling tenant, which the `tenant` scope keys on and which both
    /// fallbacks key on.
    pub tenant: Uuid,
    /// The authenticated subject, present when the caller is authenticated.
    pub subject: Option<String>,
    /// The peer address of the inbound connection, present when the platform
    /// exposes one for the connection that reached the gear. No proxying
    /// header is parsed to recover it (§1.4).
    pub peer: Option<SocketAddr>,
}

impl LimitIdentity {
    /// The identity of a request the platform authenticated and whose
    /// connection it exposed.
    #[must_use]
    pub fn new(tenant: Uuid, subject: Option<String>, peer: Option<SocketAddr>) -> Self {
        Self {
            tenant,
            subject,
            peer,
        }
    }
}

/// The verdict one check produced, which the proxy path maps to an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LimitVerdict {
    /// The request is forwarded: an admission under the `reject` or `queue`
    /// strategy, or the release of a queued request.
    Admitted,
    /// The request is forwarded under the `degrade` strategy, with the burst
    /// reserve withheld and no 429 produced for it (§1.5).
    Degraded,
    /// The request is refused with 429, carrying the header set of
    /// `cpt-cf-oagw-algo-rate-limit-headers`.
    Rejected(RateLimitHeaders),
    /// The breaker for the resolved upstream is not admitting, answered 503
    /// before any charge and before the outbound attempt.
    Open {
        /// The seconds remaining of the open interval, which the 503 carries.
        retry_after_seconds: u64,
    },
}

/// The header set of a 429 answer, and the `retry_after_seconds` member the
/// problem body carries with it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RateLimitHeaders {
    /// `X-RateLimit-Limit`, the effective sustained rate expressed per its
    /// window.
    pub limit: Option<String>,
    /// `X-RateLimit-Remaining`, the amount the counter still holds.
    pub remaining: Option<String>,
    /// `X-RateLimit-Reset`, the epoch second the counter reaches its capacity
    /// at, unset when no wall clock is available.
    pub reset: Option<String>,
    /// `Retry-After`, the whole-second delay until the counter holds the cost.
    pub retry_after: Option<String>,
    /// The `retry_after_seconds` member of the problem body, which carries the
    /// same number `Retry-After` does.
    pub retry_after_seconds: Option<u64>,
}

impl RateLimitHeaders {
    /// The pairs the 429 answer carries, in the order the flow sets them.
    #[must_use]
    pub fn pairs(&self) -> Vec<(String, String)> {
        let mut pairs = Vec::new();
        for (name, value) in [
            ("X-RateLimit-Limit", &self.limit),
            ("X-RateLimit-Remaining", &self.remaining),
            ("X-RateLimit-Reset", &self.reset),
            ("Retry-After", &self.retry_after),
        ] {
            if let Some(value) = value {
                pairs.push((String::from(name), value.clone()));
            }
        }
        pairs
    }
}

/// The resource whose `rate_limit` the effective limit came from, named as the
/// prefix of the counter key.
fn resource_of(
    limit: &EffectiveLimit,
    resolved: &ResolvedUpstream,
    matched: &MatchedRoute,
) -> (&'static str, String) {
    match limit.layer {
        LimitLayer::Route => ("route", matched.route_id.to_string()),
        LimitLayer::Upstream => ("upstream", resolved.upstream_id.to_string()),
    }
}

/// Forms the effective scope and its identifier, falling back to the `tenant`
/// scope when the configured scope's key cannot be formed (§1.4).
///
/// A `user` scope with no authenticated subject and an `ip` scope with no
/// resolvable peer address are counters the gateway cannot key, and skipping
/// enforcement would turn a configured limit into no limit, so either falls
/// back to the calling tenant's counter. The fallback is the same for every
/// request that lacks the identifier, so a caller cannot move between scopes
/// to escape a limit.
fn scope_of(
    limit: &EffectiveLimit,
    identity: &LimitIdentity,
    matched: &MatchedRoute,
) -> (RateLimitScope, String) {
    match limit.scope {
        RateLimitScope::Global => (RateLimitScope::Global, String::from("global")),
        RateLimitScope::Tenant => (RateLimitScope::Tenant, identity.tenant.to_string()),
        RateLimitScope::User => match &identity.subject {
            Some(subject) => (RateLimitScope::User, subject.clone()),
            None => (RateLimitScope::Tenant, identity.tenant.to_string()),
        },
        RateLimitScope::Ip => match identity.peer.map(|addr| addr.ip()) {
            Some(peer) => (RateLimitScope::Ip, peer.to_string()),
            None => (RateLimitScope::Tenant, identity.tenant.to_string()),
        },
        RateLimitScope::Route => (RateLimitScope::Route, matched.route_id.to_string()),
    }
}

/// The counter the effective `algorithm` selects, read and charged once,
/// against the capacity the strategy holds: the effective `burst.capacity`,
/// reduced to the sustained rate when the `degrade` strategy withholds the
/// burst reserve.
fn acquire(
    registry: &mut RateLimiterRegistry,
    key: &str,
    limit: &EffectiveLimit,
    now: Instant,
) -> AcquireOutcome {
    match limit.algorithm {
        Algorithm::TokenBucket if limit.strategy == Strategy::Degrade => {
            let bucket = registry.bucket(key, limit.burst_capacity, &limit.sustained, now);
            token_bucket_capped(bucket, limit.cost, limit.sustained.rate, now)
        }
        Algorithm::TokenBucket => {
            let bucket = registry.bucket(key, limit.burst_capacity, &limit.sustained, now);
            token_bucket(bucket, limit.cost, now)
        }
        Algorithm::SlidingWindow => {
            // The window's capacity is its rate already, so the reserve's
            // withholding leaves it unchanged (§1.5).
            let length =
                Duration::from_millis(u64::try_from(window_millis(limit.sustained.window)).unwrap_or(u64::MAX));
            let window = registry.window(key);
            sliding_window(window, limit.cost, limit.sustained.rate, length, now)
        }
    }
}

// @cpt-dod:cpt-cf-oagw-dod-rate-limit-latency:p1

/// Runs the rate-limit check one proxy request is held to.
///
/// Returns [`LimitVerdict::Admitted`] to be forwarded, [`LimitVerdict::Rejected`]
/// to be answered 429, or [`LimitVerdict::Open`] to be answered 503; the
/// `queue` strategy holds the call inside this function for no longer than its
/// wait bound, re-running the acquisition on each release it polls.
pub async fn check(
    shared: &SharedLimits,
    resolved: &ResolvedUpstream,
    matched: &MatchedRoute,
    identity: &LimitIdentity,
    now: Instant,
) -> LimitVerdict {
    // @cpt-begin:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rlc-issue
    // The check request carries the resolved upstream and route, the matched
    // route's identity, the calling tenant and subject, the peer address, and
    // the cost the effective limit charges.
    // @cpt-end:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rlc-issue

    // @cpt-begin:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rlc-none-if
    let layers = LimitLayers {
        upstream: resolved.rate_limit.as_ref(),
        route: matched.rate_limit.as_ref(),
    };
    // @cpt-begin:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rlc-none-return
    let Some(limit) = fold(&layers) else {
        // The no-limit outcome over every layer the resolution produced: the
        // request is admitted with no charge, no counter, and no rate-limit
        // header, because an unconfigured upstream is not silently limited by
        // a default it never declared, and a limit declared at any one layer
        // is enforced rather than bypassed by a guard that looked at two.
        return LimitVerdict::Admitted;
    };
    // @cpt-end:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rlc-none-return
    // @cpt-end:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rlc-none-if

    // @cpt-begin:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rlc-none-else
    // @cpt-begin:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rlc-fold
    // The effective limit and its four carried members are the fold's output,
    // produced here and carried through the rest of the check.
    // @cpt-end:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rlc-fold
    // @cpt-end:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rlc-none-else

    // The registry is only ever held inside the block below, never across the
    // await the `queue` strategy performs: one held request must never pin the
    // counter every other request is charged on.
    let (limit, key, outcome) = {
        let mut registry = shared.lock();

        // @cpt-begin:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rlc-breaker-if
        // The breaker machine for the resolved upstream is consulted before any
        // charge and before the outbound attempt.
        let breaker = registry.breaker(&upstream_prefix(resolved.upstream_id));
        if !breaker.admit(now) {
            // @cpt-begin:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rlc-breaker-return
            // RETURN 503 with the `CircuitBreakerOpen` variant, carrying
            // `retry_after_seconds` set to the seconds remaining of the open
            // interval.
            return LimitVerdict::Open {
                retry_after_seconds: breaker.retry_after_seconds(now),
            };
            // @cpt-end:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rlc-breaker-return
        }
        // @cpt-end:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rlc-breaker-if

    // @cpt-begin:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rlc-breaker-else
    // The ELSE of the breaker gate: the machine admits, so the counter is
    // charged and the request proceeds.
    // @cpt-end:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rlc-breaker-else

        // @cpt-begin:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rlc-key
        // The counter key carries the `{resource_type}:{resource_id}` prefix of
        // the resource whose `rate_limit` the effective limit came from — the
        // matched route when the effective limit is the route layer's, and the
        // resolved upstream for every other layer — followed by the effective
        // scope, its identifier, and the effective sustained window.
        let (resource_type, resource_id) = resource_of(&limit, resolved, matched);
        // @cpt-end:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rlc-key

        // @cpt-begin:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rlc-key-fallback-if
        // @cpt-begin:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rlc-key-fallback
        // A scope whose identifier the request does not carry falls back to the
        // `tenant` scope and its key rather than skip enforcement, and the
        // fallback is the same for every request that lacks the identifier.
        let (scope, scope_id) = scope_of(&limit, identity, matched);
        // @cpt-end:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rlc-key-fallback
        // @cpt-end:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rlc-key-fallback-if

        let key = RateLimiterRegistry::counter_key(
            resource_type,
            &resource_id,
            scope,
            &scope_id,
            limit.sustained.window,
        );

        // @cpt-begin:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rlc-acquire
        // The acquisition runs against the bucket or window the effective
        // algorithm selects.
        let outcome = acquire(&mut registry, &key, &limit, now);
        // @cpt-end:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rlc-acquire

        // @cpt-begin:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rlc-allow-if
        // An admitted acquisition charges the cost to the counter and hands the
        // request back to the proxy path to be forwarded, which under the
        // `degrade` strategy is the admission its own flow reports and carries
        // through to it.
        if outcome.admitted && limit.strategy != Strategy::Degrade {
            // @cpt-begin:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rlc-allow
            // The charge happened in the acquisition, and the request is handed
            // back with no rate-limit header, because ADR 0003 ties the header
            // set to the 429 answer alone.
            return LimitVerdict::Admitted;
            // @cpt-end:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rlc-allow
        }
        // @cpt-end:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rlc-allow-if

        // @cpt-begin:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rlc-allow-else
        // The refusal — or the `degrade` admission — is handed to the strategy
        // flow with the counter state and the request's cost, and the registry
        // goes out of scope here so it is never held across the wait.
        (limit, key, outcome)
        // @cpt-end:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rlc-allow-else
    };

    // @cpt-begin:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rlc-over
    let verdict = strategy(shared, &limit, &key, outcome, now).await;
    // @cpt-end:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rlc-over

    // @cpt-begin:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rlc-return
    // RETURN the admission, the over-limit answer, or the breaker answer — the
    // no-limit and breaker answers return through their own steps above — and
    // record the outcome for `cpt-cf-oagw-feature-observability` to report
    // without emitting a metric of its own: this feature registers no sink and
    // emits no metric, and the record is what that feature reads.
    tracing::debug!(
        verdict = verdict_name(&verdict),
        "the rate-limit check answered the proxy request"
    );
    verdict
    // @cpt-end:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rlc-return
}

/// The name the execution record carries for one verdict.
fn verdict_name(verdict: &LimitVerdict) -> &'static str {
    match verdict {
        LimitVerdict::Admitted => "admitted",
        LimitVerdict::Degraded => "degraded",
        LimitVerdict::Rejected(_) => "rejected",
        LimitVerdict::Open { .. } => "breaker-open",
    }
}

/// Applies the configured over-limit strategy to one refused acquisition.
///
/// This flow runs only when the check refused the acquisition — or, under
/// `degrade`, admitted it against the reduced allowance — and it produces the
/// only 429 answer in the gear.
async fn strategy(
    shared: &SharedLimits,
    limit: &EffectiveLimit,
    key: &str,
    outcome: AcquireOutcome,
    now: Instant,
) -> LimitVerdict {
    // @cpt-begin:cpt-cf-oagw-flow-rate-limit-strategy:p1:inst-rst-read
    // The effective strategy is the folded one, which is `reject` when no
    // layer declared one, that being the default ADR 0003's field table
    // declares.
    let strategy = limit.strategy;
    // @cpt-end:cpt-cf-oagw-flow-rate-limit-strategy:p1:inst-rst-read

    // @cpt-begin:cpt-cf-oagw-flow-rate-limit-strategy:p1:inst-rst-reject-if
    if strategy == Strategy::Reject {
        // @cpt-begin:cpt-cf-oagw-flow-rate-limit-strategy:p1:inst-rst-headers
        // The header set and the `retry_after_seconds` value are built from
        // the counter state at the refusal.
        let headers = rate_limit_headers(limit, &outcome, RESPONSE_HEADERS_GATE);
        // @cpt-end:cpt-cf-oagw-flow-rate-limit-strategy:p1:inst-rst-headers
        // @cpt-begin:cpt-cf-oagw-flow-rate-limit-strategy:p1:inst-rst-reject
        // RETURN 429 with the `RateLimitExceeded` variant, the header set, and
        // `X-OAGW-Error-Source: gateway`; nothing is forwarded and nothing is
        // queued.
        return LimitVerdict::Rejected(headers);
        // @cpt-end:cpt-cf-oagw-flow-rate-limit-strategy:p1:inst-rst-reject
    }
    // @cpt-end:cpt-cf-oagw-flow-rate-limit-strategy:p1:inst-rst-reject-if

    // @cpt-begin:cpt-cf-oagw-flow-rate-limit-strategy:p1:inst-rst-queue-if
    if strategy == Strategy::Queue {
        // @cpt-begin:cpt-cf-oagw-flow-rate-limit-strategy:p1:inst-rst-queue-hold
        // The slot is held for the wait bound, and the guard a cancelled wait
        // drops dequeues it silently with no charge, so a queue slot is not
        // spent on a caller that is no longer there. The registry is only ever
        // held inside the block below, never across the poll sleep.
        let mut slot = QueueSlot::new(shared, key);
        let deadline = now + QUEUE_WAIT;
        loop {
            {
                let mut registry = shared.lock();
                // @cpt-begin:cpt-cf-oagw-flow-rate-limit-strategy:p1:inst-rst-queue-full-if
                // The queue for the counter key holds its bound: the 429 of the
                // `reject` strategy is produced with no enqueueing, so the
                // bound is a property of the strategy and not a condition the
                // caller can wait out, and a caller cannot tell a full queue
                // from an exhausted bucket.
                if registry.queue_len(key) >= QUEUE_CAPACITY {
                    // @cpt-begin:cpt-cf-oagw-flow-rate-limit-strategy:p1:inst-rst-queue-full
                    slot.release(&mut registry);
                    return LimitVerdict::Rejected(rate_limit_headers(
                        limit,
                        &outcome,
                        RESPONSE_HEADERS_GATE,
                    ));
                    // @cpt-end:cpt-cf-oagw-flow-rate-limit-strategy:p1:inst-rst-queue-full
                }
                // @cpt-end:cpt-cf-oagw-flow-rate-limit-strategy:p1:inst-rst-queue-full-if

                // @cpt-begin:cpt-cf-oagw-flow-rate-limit-strategy:p1:inst-rst-queue-full-else
                // The ELSE of the bound: the request takes its slot and the
                // releases are run in the order the slots were enqueued, each
                // release re-running the check.
                // @cpt-end:cpt-cf-oagw-flow-rate-limit-strategy:p1:inst-rst-queue-full-else

                // @cpt-begin:cpt-cf-oagw-flow-rate-limit-strategy:p1:inst-rst-queue-expire-if
                if Instant::now() >= deadline {
                    // @cpt-begin:cpt-cf-oagw-flow-rate-limit-strategy:p1:inst-rst-queue-expire
                    // A request that has waited past the wait bound is answered
                    // the 429 of the `reject` strategy, dequeued, and charged
                    // nothing, so a request the queue cannot admit in time is
                    // refused rather than held.
                    slot.release(&mut registry);
                    return LimitVerdict::Rejected(rate_limit_headers(
                        limit,
                        &outcome,
                        RESPONSE_HEADERS_GATE,
                    ));
                    // @cpt-end:cpt-cf-oagw-flow-rate-limit-strategy:p1:inst-rst-queue-expire
                }
                // @cpt-end:cpt-cf-oagw-flow-rate-limit-strategy:p1:inst-rst-queue-expire-if

                // The release re-runs the check against the same counter, in the
                // order the slots were enqueued: the slot that has waited
                // longest asks first.
                let released = acquire(&mut registry, key, limit, Instant::now());
                if released.admitted {
                    slot.release(&mut registry);
                    // A released and admitted request proceeds exactly as an
                    // immediately admitted one would, with no marker that it
                    // waited.
                    return LimitVerdict::Admitted;
                }
                // @cpt-begin:cpt-cf-oagw-flow-rate-limit-strategy:p1:inst-rst-queue-recheck-if
                // @cpt-begin:cpt-cf-oagw-flow-rate-limit-strategy:p1:inst-rst-queue-recheck
                // A released request that is refused again is not enqueued a
                // second time, so the queue cannot become a retry loop and no
                // request is held indefinitely; it waits out its own slot and
                // is answered 429 by the expiry it meets.
                // @cpt-end:cpt-cf-oagw-flow-rate-limit-strategy:p1:inst-rst-queue-recheck
                // @cpt-end:cpt-cf-oagw-flow-rate-limit-strategy:p1:inst-rst-queue-recheck-if
            }
            // @cpt-begin:cpt-cf-oagw-flow-rate-limit-strategy:p1:inst-rst-queue-gone-if
            // @cpt-begin:cpt-cf-oagw-flow-rate-limit-strategy:p1:inst-rst-queue-gone
            // A client that disconnects while queued cancels this handler's
            // future, which drops the slot's guard: the slot is dequeued
            // silently, the request is charged nothing, and no answer is
            // produced, because the caller that held it is no longer there.
            // @cpt-end:cpt-cf-oagw-flow-rate-limit-strategy:p1:inst-rst-queue-gone
            // @cpt-end:cpt-cf-oagw-flow-rate-limit-strategy:p1:inst-rst-queue-gone-if
            tokio::time::sleep(QUEUE_POLL).await;
        }
        // @cpt-end:cpt-cf-oagw-flow-rate-limit-strategy:p1:inst-rst-queue-hold
    }
    // @cpt-end:cpt-cf-oagw-flow-rate-limit-strategy:p1:inst-rst-queue-if

    // @cpt-begin:cpt-cf-oagw-flow-rate-limit-strategy:p1:inst-rst-degrade-if
    // The ELSE of the strategy: `degrade`, which withholds the burst reserve
    // the effective algorithm has, the acquisition above having been measured
    // against the allowance that leaves.
    // @cpt-begin:cpt-cf-oagw-flow-rate-limit-strategy:p1:inst-rst-degrade-cover-if
    // The allowance the degraded posture leaves covers the cost: the reduced
    // capacity under `token_bucket`, and the unchanged window under
    // `sliding_window`.
    if outcome.admitted {
        // @cpt-begin:cpt-cf-oagw-flow-rate-limit-strategy:p1:inst-rst-degrade-admit
        // The charge is taken against that reduced capacity and the request is
        // handed back to be forwarded, producing no 429 and no response
        // transformation.
        return LimitVerdict::Degraded;
        // @cpt-end:cpt-cf-oagw-flow-rate-limit-strategy:p1:inst-rst-degrade-admit
    }
    // @cpt-end:cpt-cf-oagw-flow-rate-limit-strategy:p1:inst-rst-degrade-cover-if

    // @cpt-begin:cpt-cf-oagw-flow-rate-limit-strategy:p1:inst-rst-degrade-cover-else
    // @cpt-begin:cpt-cf-oagw-flow-rate-limit-strategy:p1:inst-rst-return
    // RETURN the strategy's outcome: the 429 the degraded posture's allowance
    // cannot cover, because a strategy that admitted everything would be
    // indistinguishable from no limit.
    // @cpt-begin:cpt-cf-oagw-flow-rate-limit-strategy:p1:inst-rst-degrade-refuse
    LimitVerdict::Rejected(rate_limit_headers(limit, &outcome, RESPONSE_HEADERS_GATE))
    // @cpt-end:cpt-cf-oagw-flow-rate-limit-strategy:p1:inst-rst-degrade-refuse
    // @cpt-end:cpt-cf-oagw-flow-rate-limit-strategy:p1:inst-rst-return
    // @cpt-end:cpt-cf-oagw-flow-rate-limit-strategy:p1:inst-rst-degrade-cover-else
    // @cpt-end:cpt-cf-oagw-flow-rate-limit-strategy:p1:inst-rst-degrade-if
}

/// The queue slot of one held request, whose drop dequeues it.
///
/// A client that disconnects while queued cancels the handler's future, which
/// drops the guard: the slot is dequeued silently, the request is charged
/// nothing, and no answer is produced, because the caller that held it is no
/// longer there.
struct QueueSlot<'a> {
    shared: &'a SharedLimits,
    key: String,
    released: bool,
}

impl<'a> QueueSlot<'a> {
    fn new(shared: &'a SharedLimits, key: &str) -> Self {
        Self {
            shared,
            key: String::from(key),
            released: false,
        }
    }

    /// Dequeues the slot, marking it released so the drop that follows does
    /// not dequeue a second one.
    fn release(&mut self, registry: &mut RateLimiterRegistry) {
        self.released = true;
        registry.dequeue(&self.key);
    }
}

impl Drop for QueueSlot<'_> {
    fn drop(&mut self) {
        if !self.released {
            self.shared.lock().dequeue(&self.key);
        }
    }
}

/// Builds the header set of a 429 answer from the counter state at the
/// refusal.
///
/// The set is produced for a refusal and for nothing else, and the gate the
/// `response_headers` member sets closes it entirely, headers and guidance
/// with them.
#[must_use]
pub fn rate_limit_headers(
    limit: &EffectiveLimit,
    outcome: &AcquireOutcome,
    gate: bool,
) -> RateLimitHeaders {
    // @cpt-begin:cpt-cf-oagw-algo-rate-limit-headers:p1:inst-hdr-gate-if
    if !gate {
        // @cpt-begin:cpt-cf-oagw-algo-rate-limit-headers:p1:inst-hdr-gate
        // RETURN the empty set, and with it no `retry_after_seconds` member, so
        // a deployment that withholds the headers withholds the guidance with
        // them.
        return RateLimitHeaders::default();
        // @cpt-end:cpt-cf-oagw-algo-rate-limit-headers:p1:inst-hdr-gate
    }
    // @cpt-end:cpt-cf-oagw-algo-rate-limit-headers:p1:inst-hdr-gate-if

    // @cpt-begin:cpt-cf-oagw-algo-rate-limit-headers:p1:inst-hdr-limit
    // The effective sustained rate expressed per its window, which is the set
    // ADR 0003's More Information section shows.
    let limit_header = limit.sustained.rate.to_string();
    // @cpt-end:cpt-cf-oagw-algo-rate-limit-headers:p1:inst-hdr-limit

    // @cpt-begin:cpt-cf-oagw-algo-rate-limit-headers:p1:inst-hdr-remaining
    // The amount the counter still holds — the tokens left in the bucket under
    // `token_bucket`, and the sustained rate minus the charged total in the
    // current window under `sliding_window` — which the refusal's own outcome
    // reports in the currency of the algorithm that produced it.
    let remaining = outcome.remaining.to_string();
    // @cpt-end:cpt-cf-oagw-algo-rate-limit-headers:p1:inst-hdr-remaining

    // @cpt-begin:cpt-cf-oagw-algo-rate-limit-headers:p1:inst-hdr-reset
    // The epoch second at which the counter reaches its capacity, read from
    // the wall clock: the instant the bucket is full again under
    // `token_bucket`, and the instant the oldest charge ages out of the window
    // under `sliding_window`. A wall clock that is unavailable leaves the
    // header unset and the other three intact.
    let reset = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|now| (now.as_secs().saturating_add(outcome.reset_seconds)).to_string());
    // @cpt-end:cpt-cf-oagw-algo-rate-limit-headers:p1:inst-hdr-reset

    // @cpt-begin:cpt-cf-oagw-algo-rate-limit-headers:p1:inst-hdr-retry
    // The whole-second delay until the counter holds the request's `cost`,
    // rounded up to at least 1, carried in `Retry-After` and in the problem
    // body's `retry_after_seconds` member as the same number.
    let retry = outcome.delay_seconds.max(1);
    // @cpt-end:cpt-cf-oagw-algo-rate-limit-headers:p1:inst-hdr-retry

    // @cpt-begin:cpt-cf-oagw-algo-rate-limit-headers:p1:inst-hdr-return
    // RETURN the header set and the value.
    RateLimitHeaders {
        limit: Some(limit_header),
        remaining: Some(remaining),
        reset,
        retry_after: Some(retry.to_string()),
        retry_after_seconds: Some(retry),
    }
    // @cpt-end:cpt-cf-oagw-algo-rate-limit-headers:p1:inst-hdr-return
}

/// The breaker prefix of a resolved upstream, which is the key the breaker
/// machine is held under and the prefix the cleanup drops.
#[must_use]
pub fn upstream_prefix(upstream_id: Uuid) -> String {
    format!("upstream:{upstream_id}")
}

// @cpt-dod:cpt-cf-oagw-dod-rate-limit-state:p1

/// Runs the cleanup a successful upstream or route deletion notifies, dropping
/// every entry keyed under the deleted configuration's prefix.
///
/// This flow runs on the management write path and not on the proxy path, so
/// it is the one flow in the feature no proxy request triggers, and it
/// completes before the delete's response is produced.
pub fn cleanup(
    registry: &mut RateLimiterRegistry,
    resource_type: &str,
    resource_id: Uuid,
) -> usize {
    // @cpt-begin:cpt-cf-oagw-flow-rate-limit-cleanup:p2:inst-rcu-notify
    // The notification carries the resource type and identifier of the row the
    // write path deleted, in process and before the delete's response is
    // produced.
    // @cpt-end:cpt-cf-oagw-flow-rate-limit-cleanup:p2:inst-rcu-notify

    // @cpt-begin:cpt-cf-oagw-flow-rate-limit-cleanup:p2:inst-rcu-upstream-if
    if resource_type == "upstream" {
        // @cpt-begin:cpt-cf-oagw-flow-rate-limit-cleanup:p2:inst-rcu-upstream-drop
        // Drop every entry whose key begins with that upstream's prefix,
        // including the breaker machine held for it, and retain nothing.
        let dropped = registry.drop_prefix(&upstream_prefix(resource_id));
        // @cpt-end:cpt-cf-oagw-flow-rate-limit-cleanup:p2:inst-rcu-upstream-drop

        // @cpt-begin:cpt-cf-oagw-flow-rate-limit-cleanup:p2:inst-rcu-return
        // RETURN the number of entries dropped, which is a diagnostic value
        // and not a condition any caller branches on.
        return dropped;
        // @cpt-end:cpt-cf-oagw-flow-rate-limit-cleanup:p2:inst-rcu-return
    }
    // @cpt-end:cpt-cf-oagw-flow-rate-limit-cleanup:p2:inst-rcu-upstream-if

    // @cpt-begin:cpt-cf-oagw-flow-rate-limit-cleanup:p2:inst-rcu-route-if
    if resource_type == "route" {
        // @cpt-begin:cpt-cf-oagw-flow-rate-limit-cleanup:p2:inst-rcu-route-drop
        // Drop every entry whose key begins with that route's prefix and
        // retain the upstream's own buckets and its breaker machine, because
        // the route's counters are not the upstream's.
        let dropped = registry.drop_prefix(&format!("route:{resource_id}"));
        // @cpt-end:cpt-cf-oagw-flow-rate-limit-cleanup:p2:inst-rcu-route-drop

        // @cpt-begin:cpt-cf-oagw-flow-rate-limit-cleanup:p2:inst-rcu-return
        // RETURN the number of entries dropped.
        return dropped;
        // @cpt-end:cpt-cf-oagw-flow-rate-limit-cleanup:p2:inst-rcu-return
    }
    // @cpt-end:cpt-cf-oagw-flow-rate-limit-cleanup:p2:inst-rcu-route-if

    // @cpt-begin:cpt-cf-oagw-flow-rate-limit-cleanup:p2:inst-rcu-else
    // @cpt-begin:cpt-cf-oagw-flow-rate-limit-cleanup:p2:inst-rcu-none
    // A notification that names no resource of the two is not a cleanup
    // instruction, and a deletion that holds no bucket drops nothing, because
    // the cleanup is idempotent over an absent key set.
    // @cpt-end:cpt-cf-oagw-flow-rate-limit-cleanup:p2:inst-rcu-none
    // @cpt-end:cpt-cf-oagw-flow-rate-limit-cleanup:p2:inst-rcu-else

    // @cpt-begin:cpt-cf-oagw-flow-rate-limit-cleanup:p2:inst-rcu-return
    0
    // @cpt-end:cpt-cf-oagw-flow-rate-limit-cleanup:p2:inst-rcu-return
}

/// The observer the write path of the configuration feature notifies, which
/// runs the cleanup over the registry it holds.
pub struct RegistryCleanup {
    registry: SharedLimits,
}

impl RegistryCleanup {
    /// The observer over one shared registry.
    #[must_use]
    pub fn new(registry: SharedLimits) -> Self {
        Self { registry }
    }
}

impl crate::control_plane::cache::RateLimitCleanup for RegistryCleanup {
    fn upstream_deleted(&self, _tenant_id: Uuid, upstream_id: Uuid) {
        let mut registry = self.registry.lock();
        let _ = cleanup(&mut registry, "upstream", upstream_id);
    }

    fn route_deleted(&self, _tenant_id: Uuid, route_id: Uuid) {
        let mut registry = self.registry.lock();
        let _ = cleanup(&mut registry, "route", route_id);
    }
}
