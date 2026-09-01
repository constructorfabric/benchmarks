// Created: 2026-08-31 by Constructor Tech
//! Circuit breaker of the proxy data plane (PRD `cpt-cf-oagw-nfr-high-availability`).
//!
//! # The contract
//!
//! "Circuit breakers MUST prevent cascade failures from unhealthy upstreams.
//! Threshold: 99.9% uptime; circuit breaker trips within 5 failed requests in
//! 30s window." The defaults of [`crate::config::CircuitBreakerConfig`] are that
//! threshold and that window, and the window is what the PRD says it is — the
//! failures the instance observed inside the last 30s, whatever happened
//! between them — so a deployment that configures nothing gets the behaviour
//! the PRD asks for.
//!
//! # The machine
//!
//! `CLOSED → OPEN → HALF_OPEN → CLOSED`, with `HALF_OPEN → OPEN` when the probe
//! fails:
//!
//! | state   | what a request sees |
//! |---|---|
//! | `closed` | the request is dialled; failures accumulate in the window |
//! | `open` | the request is refused 503 `circuit_breaker.open.v1` without a dial, with the remaining cooldown as `Retry-After` |
//! | `half_open` | the one probe is dialled; every other request is refused with the same answer |
//!
//! # The window
//!
//! The window slides and it is the only thing that forgets. A failure is
//! stamped with the instant its report arrived and stops counting once it is
//! older than the window, which is what keeps a recovered upstream from staying
//! one failure away from a trip forever. A success is **not** a failure and it
//! does not empty the window either: an upstream that answers 200 on its cheap
//! requests and 500 on its expensive ones is the partial failure the breaker
//! exists to stop, and wiping the window on every 200 would leave it
//! untrippable at any failure rate.
//!
//! # Key
//!
//! One breaker per **upstream id**, per [`crate::domain::proxy::ProxyService`]
//! instance. The id and not the alias, for three reasons: two tenants may own
//! the same alias and must not trip each other's breaker; renaming an alias must
//! not reset a trip that is still cooling down; and the removal seam
//! (`UpstreamRemoval`) already forgets per-upstream state by id when a record is
//! deleted, so a recreated upstream starts closed. What the *operator* reads is
//! the `host` label of [`crate::infra::metrics`], and that label is the alias of
//! the very upstream the id names — the state and the label are one upstream,
//! which is why the label is carried by the caller and not looked up here.
//!
//! The state is per instance, like the token buckets of ADR-0003
//! "Distribution": a gear instance owns its own view of upstream health, and no
//! cross-instance sync is built. Two instances behind a load balancer each trip
//! on their own failures, which is the intended behaviour of a breaker: each
//! stops sending traffic it can see failing.
//!
//! # What a failure is
//!
//! [`is_health_failure`] is the one exhaustive decision, over all of
//! [`OagwErrorKind`] so that a new kind cannot silently join either side. What
//! it can be handed depends on the stage that observed the request, and both
//! stages report:
//!
//! | stage | the observer | the failure kinds it can deliver |
//! |---|---|---|
//! | the head | `send` / `send_handshake` | `LinkUnavailable`, `RequestTimeout` (a stalled connection surfaces as `timeout.request.v1`, not as `timeout.connection.v1`), `PayloadTooLarge`, `ProtocolError`, `Internal`, `Validation` |
//! | the body | `forward_body` | `IdleTimeout`, `RequestTimeout` (the overall body budget), `StreamAborted` |
//!
//! The head observer sits *behind* the dial, so the gateway-side kinds it can
//! carry — the method the proxy cannot forward, a header it cannot render —
//! belong to a dial that never happened, and they classify as **not** a health
//! failure for that reason. Everything else that is not the upstream's health is
//! the client's error (a 4xx), a refusal the gateway made before the dial (CORS,
//! rate limit, an oversized request body, the egress policy, framing) or a
//! dependency of the *gateway* that is missing (a credential, a bound plugin).
//!
//! Two kinds are classified as failures and no path produces them today:
//! `ConnectionTimeout`, which the outbound client folds into the head budget,
//! and `DownstreamError`, which the data plane never constructs. They stay on
//! the failure side because that is what they mean, and the match is exhaustive,
//! so a path that starts producing one cannot silently under-count the upstream
//! it broke.
//!
//! # Half-open fairness
//!
//! Exactly one probe is admitted per cooldown: the request that finds the
//! breaker `half_open` with no probe in flight becomes the probe, and the
//! requests that arrive while it is on the wire are refused. The role is
//! **carried, not re-derived**: [`CircuitBreakers::admit`] mints a [`Probe`]
//! token for the request it admits as the probe and [`CircuitBreakers::record`]
//! honours only the token the breaker is still holding, so a request that was
//! dialled while the breaker was closed and reports after the trip — or the late
//! report of a probe that has already been replaced — cannot close or re-open a
//! breaker it did not probe.
//!
//! A probe that outlives its budget is treated as abandoned and replaced, so a
//! cancelled request cannot leave every later request refused forever: the head
//! budget bounds the probe, which is why [`CircuitBreakers::new`] takes it. A
//! request admitted as the probe that never reaches its dial — a framing refusal
//! after the admission, say — reports nothing and waits out the same budget.
//!
//! # The clock
//!
//! Every `Instant` the breaker compares is **passed in**, not read: `admit` and
//! `record` take the `now` of the request they serve. That is what makes the
//! window, the cooldown and the probe budget testable without a real wait. The
//! data plane passes [`std::time::Instant::now`], deliberately the `std` clock
//! and not tokio's, so a mocked tokio clock cannot move a breaker.

use std::collections::VecDeque;
use std::time::Duration;
use std::time::Instant;

use dashmap::DashMap;
use http::StatusCode;
use uuid::Uuid;

use crate::error::OagwErrorKind;
use crate::infra::metrics;

/// Initial capacity of the breaker map, as a hint to [`DashMap::with_capacity`].
///
/// It is a hint and **not** a ceiling: nothing checks the map's length. The map
/// is bounded by what it resolves records from — one breaker per upstream id,
/// and an alias that resolves to nothing never reaches the breaker — and the
/// removal seam drops the entry of a deleted record, so an instance does not
/// accumulate breakers for upstreams it no longer has. A store holding more
/// upstreams than this simply grows the map.
const INITIAL_BREAKER_CAPACITY: usize = 65_536;

/// A breaker state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum State {
    /// Dialling is allowed; failures are counted.
    Closed,
    /// Nothing is dialled until the cooldown is over.
    Open,
    /// The cooldown is over; one probe is allowed.
    HalfOpen,
}

impl State {
    /// Label of the state, as the transition metric and the log spell it.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            State::Closed => "closed",
            State::Open => "open",
            State::HalfOpen => "half_open",
        }
    }

    /// Value the state gauge reports.
    ///
    /// `open` is the largest value because it is the state an operator alerts
    /// on: a threshold of `>= 2` reads "the upstream is unreachable".
    pub(crate) const fn value(self) -> u64 {
        match self {
            State::Closed => 0,
            State::HalfOpen => 1,
            State::Open => 2,
        }
    }
}

/// Proof that a request is the probe its breaker is waiting for.
///
/// [`CircuitBreakers::admit`] mints one when it promotes an open breaker to
/// half-open. It names the admission it belongs to by the instant that admission
/// was taken, so a request that was admitted as a probe and reports only after
/// its breaker has replaced that admission — a cancelled dial, a probe that
/// outlived its budget — can no longer claim the slot of the probe that
/// replaced it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Probe {
    started_at: Instant,
}

/// What a request may do with its upstream.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Admit {
    /// Dial the upstream: the breaker is closed, or it is switched off.
    Dial,
    /// Dial the upstream **as the probe** of a half-open breaker. The token is
    /// what the request reports back, and what proves the role it was given.
    Probe(Probe),
    /// Refuse without dialling, naming the seconds still to wait.
    Refuse {
        /// Cooldown, or probe budget, still to run, rounded up to a whole second.
        retry_after_secs: u64,
    },
}

impl Admit {
    /// The probe this admission granted, if it granted one.
    #[must_use]
    pub(crate) fn probe(self) -> Option<Probe> {
        match self {
            Admit::Probe(token) => Some(token),
            Admit::Dial | Admit::Refuse { .. } => None,
        }
    }
}

/// What one request observed of its upstream, and at which stage.
///
/// The head and the body are two observers of one request: the head reports the
/// status the upstream answered with, or why no head ever came; the body reports
/// a transfer that never finished. Both are evidence about the same upstream,
/// which is why both are reported and both are judged by
/// [`is_health_failure`].
#[derive(Clone, Copy, Debug)]
pub(crate) enum Observed {
    /// The upstream answered with this status. Its body may still fail, and the
    /// body then reports a second time for the same request.
    Answered(StatusCode),
    /// No usable answer: the dial failed before a status existed, or the body
    /// never finished. `kind` names why.
    Failed(OagwErrorKind),
}

/// What a forwarded body reports to its upstream's breaker.
///
/// The body outlives the request that dialled it, so what it reports to has to
/// be owned: the breakers behind an `Arc`, the upstream the body came from, the
/// probe role that request was admitted as and the instruments the transition
/// emits to. Cloned once per response, so a body that fails reports without the
/// breaker's own map having to be reachable from the stream.
#[derive(Clone)]
pub(crate) struct BodyReport {
    breakers: std::sync::Arc<CircuitBreakers>,
    upstream_id: Uuid,
    host: String,
    probe: Option<Probe>,
    metrics: metrics::ProxyMetrics,
}

impl BodyReport {
    /// The report the forwarded body of `upstream` carries.
    #[must_use]
    pub(crate) fn for_response(
        breakers: std::sync::Arc<CircuitBreakers>,
        upstream: &crate::domain::model::Upstream,
        probe: Option<Probe>,
        metrics: metrics::ProxyMetrics,
    ) -> Self {
        Self {
            breakers,
            upstream_id: upstream.id,
            host: upstream.alias.clone(),
            probe,
            metrics,
        }
    }

    /// Report that the body of the request never finished, with `kind` naming
    /// why.
    ///
    /// The head of this request has already reported the status the upstream
    /// answered with, so this is the second observation of one request: a 200
    /// head followed by a body the upstream never finished is the slow-upstream
    /// failure the head cannot see, and the breaker has to count it.
    pub(crate) fn observe(&self, kind: OagwErrorKind) {
        self.breakers.record(
            self.upstream_id,
            &self.host,
            self.probe,
            Observed::Failed(kind),
            Instant::now(),
            &self.metrics,
        );
    }
}

/// The breakers of the data plane, one per upstream id.
pub(crate) struct CircuitBreakers {
    /// Whether the breaker is consulted at all. Off means every request is
    /// dialled and no state is kept, so a deployment that turns the breaker off
    /// pays nothing for it.
    enabled: bool,
    /// Failures inside the window that trip a closed breaker.
    threshold: usize,
    /// How long a failure stays counted.
    window: Duration,
    /// How long an open breaker stays open.
    cooldown: Duration,
    /// How long a probe may take before it is treated as abandoned.
    probe_budget: Duration,
    breakers: DashMap<Uuid, Breaker>,
}

impl std::fmt::Debug for CircuitBreakers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CircuitBreakers")
            .field("enabled", &self.enabled)
            .finish_non_exhaustive()
    }
}

/// State of one upstream's breaker.
#[derive(Default)]
struct Breaker {
    state: State,
    /// Failures still inside the window, oldest first.
    failures: VecDeque<Instant>,
    /// When the current cooldown ends; meaningful while open.
    cooldown_ends_at: Option<Instant>,
    /// When the probe in flight was admitted; meaningful while half-open.
    probe_started_at: Option<Instant>,
}

impl Default for State {
    /// A breaker starts closed: an upstream that was never dialled is neither
    /// refused nor suspected. Spelled rather than derived, so a state added
    /// later cannot become the silent default.
    fn default() -> Self {
        State::Closed
    }
}

impl CircuitBreakers {
    /// Build the breakers of one data plane.
    pub(crate) fn new(
        config: &crate::config::CircuitBreakerConfig,
        probe_budget: Duration,
    ) -> Self {
        // A threshold below one, or a window of no length, would make the
        // breaker trip on nothing or never: both are a typo, and a typo in a
        // safety switch must not silently disable it. One failure in a one
        // second window is the smallest honest reading of "5 in 30s".
        let threshold = usize::try_from(config.failure_threshold)
            .unwrap_or(usize::MAX)
            .max(1);
        Self {
            enabled: config.enabled,
            threshold,
            window: Duration::from_secs(config.failure_window_secs.max(1)),
            cooldown: Duration::from_secs(config.cooldown_secs),
            probe_budget,
            breakers: DashMap::with_capacity(INITIAL_BREAKER_CAPACITY.min(64)),
        }
    }

    /// Ask whether a request to `upstream_id` may dial.
    ///
    /// The answer is taken before the dial, and a probe it admits is minted
    /// here: the requests that arrive behind a probe are refused until it has
    /// reported, and the one that is admitted as the probe holds the token it
    /// has to report with.
    ///
    /// `now` is the instant of the request, injected by the caller (see the
    /// module docs on the clock).
    pub(crate) fn admit(
        &self,
        upstream_id: Uuid,
        host: &str,
        now: Instant,
        metrics: &metrics::ProxyMetrics,
    ) -> Admit {
        if !self.enabled {
            return Admit::Dial;
        }
        let mut promoted = None;
        let admit = {
            let mut entry = self.breakers.entry(upstream_id).or_default();
            let breaker = entry.value_mut();
            match breaker.state {
                State::Closed => Admit::Dial,
                State::Open if breaker.cooldown_over(now) => {
                    promoted = Some((State::Open, State::HalfOpen));
                    breaker.state = State::HalfOpen;
                    breaker.probe_started_at = Some(now);
                    Admit::Probe(Probe { started_at: now })
                }
                State::Open => Admit::Refuse {
                    retry_after_secs: retry_after(breaker.cooldown_remaining(now)),
                },
                State::HalfOpen if breaker.probe_abandoned(now, self.probe_budget) => {
                    // The probe never reported, so its slot is free again and
                    // this request takes it. The token the abandoned probe still
                    // carries names an admission that is no longer in flight,
                    // which is why its report, if it ever arrives, moves
                    // nothing.
                    breaker.probe_started_at = Some(now);
                    Admit::Probe(Probe { started_at: now })
                }
                State::HalfOpen => Admit::Refuse {
                    retry_after_secs: retry_after(breaker.probe_remaining(now, self.probe_budget)),
                },
            }
        };
        if let Some((from, to)) = promoted {
            Self::transition(upstream_id, host, from, to, metrics);
        }
        admit
    }

    /// Report the outcome of one dialled request.
    ///
    /// Only a request that dialled reports, and only the one holding the probe
    /// token its breaker is still waiting for may move a half-open breaker. A
    /// refusal the gateway answered without dialling reports nothing, and
    /// neither does a request that was dialled before the breaker tripped and
    /// only reports afterwards.
    ///
    /// `observed` is what the request saw, `now` the instant of the report.
    pub(crate) fn record(
        &self,
        upstream_id: Uuid,
        host: &str,
        probe: Option<Probe>,
        observed: Observed,
        now: Instant,
        metrics: &metrics::ProxyMetrics,
    ) {
        if !self.enabled {
            return;
        }
        let failure = is_health_failure(observed);
        let mut transition = None;
        {
            let mut entry = self.breakers.entry(upstream_id).or_default();
            let breaker = entry.value_mut();
            match breaker.state {
                State::Closed => {
                    // The window is the breaker's memory of the failures inside
                    // it, and it is the only thing that forgets: a success is
                    // not a failure and does not empty it (PRD "5 failed
                    // requests in 30s window"), while a failure older than the
                    // window is no longer evidence.
                    if failure {
                        breaker.failures.push_back(now);
                    }
                    breaker.trim(self.window, now);
                    if breaker.failures.len() >= self.threshold {
                        breaker.state = State::Open;
                        breaker.cooldown_ends_at = Some(now + self.cooldown);
                        // The trip consumed the window's answer: the breaker is
                        // open, and what it owes from here is the cooldown.
                        breaker.failures.clear();
                        transition = Some((State::Closed, State::Open));
                    }
                }
                State::HalfOpen if breaker.holds(probe) => {
                    breaker.probe_started_at = None;
                    if failure {
                        breaker.state = State::Open;
                        breaker.cooldown_ends_at = Some(now + self.cooldown);
                        transition = Some((State::HalfOpen, State::Open));
                    } else {
                        breaker.state = State::Closed;
                        transition = Some((State::HalfOpen, State::Closed));
                    }
                }
                // Open: nothing was dialled. Half-open without the token the
                // breaker holds: a request dialled while the breaker was closed,
                // reporting after the trip, or the late report of an abandoned
                // probe. Neither one decides the state of the breaker.
                State::Open | State::HalfOpen => {}
            }
        }
        if let Some((from, to)) = transition {
            Self::transition(upstream_id, host, from, to, metrics);
        }
    }

    /// Drop the breaker of a deleted upstream.
    ///
    /// A recreated upstream starts closed: the record it replaced is gone, and
    /// so is the health this instance saw of it.
    pub(crate) fn forget(&self, upstream_id: Uuid) {
        self.breakers.remove(&upstream_id);
    }

    /// Move a breaker between two states and report it (DESIGN §4.2, §4.3).
    fn transition(
        upstream_id: Uuid,
        host: &str,
        from: State,
        to: State,
        metrics: &metrics::ProxyMetrics,
    ) {
        metrics.breaker_transition(host, from.label(), to.label());
        metrics.breaker_state(host, to.value());
        // A transition is an event of DESIGN §4.3 and the level it names for a
        // breaker that opened is `WARN`: an operator has to see the upstream
        // was cut off, and every transition is rare enough to log.
        tracing::warn!(
            upstream_id = %upstream_id,
            host = %host,
            from_state = from.label(),
            to_state = to.label(),
            "circuit breaker state changed"
        );
    }
}

impl Breaker {
    /// Drop the failures that fell out of the window.
    ///
    /// The deque never holds more than a trip needs: once the breaker is open
    /// the window is empty anyway, and while it is closed a failure older than
    /// the window is no longer evidence.
    fn trim(&mut self, window: Duration, now: Instant) {
        while self
            .failures
            .front()
            .is_some_and(|at| now.duration_since(*at) > window)
        {
            self.failures.pop_front();
        }
    }

    /// Whether the cooldown has run out.
    fn cooldown_over(&self, now: Instant) -> bool {
        self.cooldown_ends_at.is_none_or(|ends_at| ends_at <= now)
    }

    /// Time still to wait before the breaker may be probed.
    fn cooldown_remaining(&self, now: Instant) -> Duration {
        self.cooldown_ends_at.map_or(Duration::ZERO, |ends_at| {
            ends_at.saturating_duration_since(now)
        })
    }

    /// Whether the probe in flight can no longer report.
    fn probe_abandoned(&self, now: Instant, budget: Duration) -> bool {
        self.probe_started_at
            .is_some_and(|started| now.duration_since(started) > budget)
    }

    /// Time still to wait before a new probe would be admitted.
    fn probe_remaining(&self, now: Instant, budget: Duration) -> Duration {
        self.probe_started_at.map_or(budget, |started| {
            budget.saturating_sub(now.duration_since(started))
        })
    }

    /// Whether `probe` is the token this breaker is waiting for.
    fn holds(&self, probe: Option<Probe>) -> bool {
        self.probe_started_at == probe.map(|token| token.started_at)
    }
}

/// Whether one request left its upstream looking unhealthy.
///
/// This is the one place the breaker decides what a failure is, and it is
/// exhaustive over [`OagwErrorKind`] so that a new kind cannot silently join
/// either side. `observed` is what the request saw, at whichever stage it
/// reported: the status the upstream answered with, or the reason no usable
/// answer came (see the module docs for which stage delivers which kind).
///
/// PRD `cpt-cf-oagw-nfr-high-availability` counts "5 failed requests in 30s
/// window" towards a trip, and a *failed* request here is an upstream-side
/// health failure and nothing else:
///
/// * a **5xx** the upstream answered — the upstream is there and says it is
///   broken;
/// * a **dial failure, a timeout or an abort** — the upstream could not be
///   reached, or the answer it started never finished: `LinkUnavailable`,
///   `ConnectionTimeout`, `RequestTimeout`, `IdleTimeout`, `StreamAborted`,
///   `DownstreamError`, `ProtocolError`.
///
/// Everything else is **not** the upstream's health:
///
/// * a **4xx** is the client's error and the upstream answered it — counting it
///   would let one misconfigured client trip a healthy upstream's breaker;
/// * a **gateway refusal** — CORS, a rate limit, an oversized request body, the
///   egress policy, framing validation — happened before the dial, so the
///   upstream was never asked;
/// * a **credential or plugin failure** — `AuthenticationFailed`,
///   `SecretNotFound`, `PluginNotFound` — is a dependency of the *gateway*
///   missing, which an operator fixes in the configuration, not by waiting for
///   the upstream to recover;
/// * `CircuitBreakerOpen` itself never reaches this decision, because an open
///   breaker refuses without dialling and an un-dialled request reports nothing.
pub(crate) fn is_health_failure(observed: Observed) -> bool {
    match observed {
        Observed::Answered(status) => status.is_server_error(),
        Observed::Failed(kind) => match kind {
            // The upstream could not be reached, or the answer it started never
            // finished. `ConnectionTimeout` and `DownstreamError` are produced
            // by no path today (see the module docs); they stay failures
            // because that is what they mean.
            OagwErrorKind::LinkUnavailable
            | OagwErrorKind::ConnectionTimeout
            | OagwErrorKind::RequestTimeout
            | OagwErrorKind::IdleTimeout
            | OagwErrorKind::StreamAborted
            | OagwErrorKind::DownstreamError
            | OagwErrorKind::ProtocolError => true,
            // Nothing the gateway decided before the dial, nothing the client
            // got wrong and nothing of the gateway's own dependencies says
            // anything about the upstream: the refusals of the gateway, the
            // client's mistakes and the gateway's own missing dependencies,
            // and an un-dialled request leaves the breaker exactly as it was.
            OagwErrorKind::Validation
            | OagwErrorKind::MissingTargetHost
            | OagwErrorKind::InvalidTargetHost
            | OagwErrorKind::UnknownTargetHost
            | OagwErrorKind::NotFound
            | OagwErrorKind::PayloadTooLarge
            | OagwErrorKind::RateLimitExceeded
            | OagwErrorKind::CorsOriginNotAllowed
            | OagwErrorKind::CorsMethodNotAllowed
            | OagwErrorKind::AliasConflict
            | OagwErrorKind::RouteConflict
            | OagwErrorKind::PluginConflict
            | OagwErrorKind::PluginInUse
            | OagwErrorKind::AuthenticationFailed
            | OagwErrorKind::SecretNotFound
            | OagwErrorKind::PluginNotFound
            | OagwErrorKind::Internal
            | OagwErrorKind::CircuitBreakerOpen => false,
        },
    }
}

/// `Retry-After` of a refusal, in whole seconds from `remaining`.
///
/// `remaining` rounded **up**, and never zero — not even at the exact end of a
/// budget, where a refusal still has to name a second: a refusal that says
/// "retry now" is not guidance, it is noise. Rounding up rather than down is
/// what makes the guidance honest the other way too: a client that waits the
/// seconds it was named is always *past* the deadline, never a fraction short
/// of it and refused a second time.
fn retry_after(remaining: Duration) -> u64 {
    (remaining.as_secs() + u64::from(remaining.subsec_nanos() != 0)).max(1)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Admit, CircuitBreakers, Observed, State, is_health_failure, retry_after};
    use crate::config::CircuitBreakerConfig;
    use crate::error::{OagwError, OagwErrorKind, ResourceKind};
    use crate::infra::metrics::ProxyMetrics;

    /// The probe budget of the unit tests, and the answer of the refusal that
    /// names it whole.
    const BUDGET: u64 = 30;

    /// A breaker with the default 30s failure window.
    fn wrap(config: CircuitBreakerConfig) -> CircuitBreakers {
        CircuitBreakers::new(&config, Duration::from_secs(BUDGET))
    }

    /// A breaker that trips on `threshold` failures and probes after
    /// `cooldown` seconds.
    fn breakers(threshold: u32, cooldown: u64) -> CircuitBreakers {
        wrap(CircuitBreakerConfig {
            failure_threshold: threshold,
            cooldown_secs: cooldown,
            ..CircuitBreakerConfig::default()
        })
    }

    /// `start + secs`: how the tests spell an instant, so the arithmetic the
    /// assertions read is the arithmetic the code does.
    fn at(start: std::time::Instant, secs: u64) -> std::time::Instant {
        start + Duration::from_secs(secs)
    }

    /// Report `kind` as the outcome of a request that was not the probe.
    fn report(
        breaker: &CircuitBreakers,
        id: uuid::Uuid,
        at: std::time::Instant,
        kind: OagwErrorKind,
    ) {
        let metrics = ProxyMetrics::from_global();
        breaker.record(
            id,
            "api.vendor.com",
            None,
            Observed::Failed(kind),
            at,
            &metrics,
        );
    }

    /// Report `status` as the answer a request that was not the probe got.
    fn answer(breaker: &CircuitBreakers, id: uuid::Uuid, at: std::time::Instant, status: u16) {
        let metrics = ProxyMetrics::from_global();
        let observed = Observed::Answered(
            http::StatusCode::from_u16(status).expect("a test status is a valid status"),
        );
        breaker.record(id, "api.vendor.com", None, observed, at, &metrics);
    }

    fn admit(breaker: &CircuitBreakers, id: uuid::Uuid, at: std::time::Instant) -> Admit {
        breaker.admit(id, "api.vendor.com", at, &ProxyMetrics::from_global())
    }

    #[test]
    fn the_prd_threshold_trips_on_the_fifth_failure() {
        let breaker = breakers(5, 30);
        let id = uuid::Uuid::now_v7();
        let start = std::time::Instant::now();
        for step in 0..4 {
            report(
                &breaker,
                id,
                at(start, step),
                OagwErrorKind::LinkUnavailable,
            );
        }
        assert_eq!(
            admit(&breaker, id, at(start, 4)),
            Admit::Dial,
            "four failures are one short of the PRD threshold"
        );
        report(&breaker, id, at(start, 4), OagwErrorKind::LinkUnavailable);
        assert_eq!(
            admit(&breaker, id, at(start, 5)),
            Admit::Refuse {
                retry_after_secs: 29
            },
            "the fifth failure in the window opens the breaker for the 29s of \
             cooldown left"
        );
    }

    /// The window, not the streak: five failures inside it trip the breaker
    /// whatever happened between them, which is what makes a partially failing
    /// upstream — 200 on its cheap requests, 500 on its expensive ones —
    /// trappable.
    #[test]
    fn five_failures_in_the_window_trip_even_between_successes() {
        let breaker = breakers(5, 30);
        let id = uuid::Uuid::now_v7();
        let start = std::time::Instant::now();
        for step in 0..5 {
            report(
                &breaker,
                id,
                at(start, step),
                OagwErrorKind::LinkUnavailable,
            );
            answer(&breaker, id, at(start, step), 200);
        }
        assert_eq!(
            admit(&breaker, id, at(start, 5)),
            Admit::Refuse {
                retry_after_secs: 29
            },
            "five failures inside the window, one success between each of them"
        );
    }

    /// A failure leaves the window by aging out and by nothing else, so an
    /// upstream that failed once and recovered is not left one failure away
    /// from a trip forever.
    #[test]
    fn a_failure_leaves_the_window_when_it_ages_out() {
        let breaker = wrap(CircuitBreakerConfig {
            failure_threshold: 2,
            failure_window_secs: 1,
            cooldown_secs: 30,
            ..CircuitBreakerConfig::default()
        });
        let id = uuid::Uuid::now_v7();
        let start = std::time::Instant::now();
        report(&breaker, id, start, OagwErrorKind::LinkUnavailable);
        // Two seconds later the first failure is out of the one second window,
        // so this second failure is alone in it.
        report(&breaker, id, at(start, 2), OagwErrorKind::LinkUnavailable);
        assert_eq!(
            admit(&breaker, id, at(start, 2)),
            Admit::Dial,
            "the failure that aged out no longer counts"
        );
    }

    #[test]
    fn a_disabled_breaker_neither_refuses_nor_keeps_state() {
        let breaker = wrap(CircuitBreakerConfig {
            enabled: false,
            ..CircuitBreakerConfig::default()
        });
        let id = uuid::Uuid::now_v7();
        let start = std::time::Instant::now();
        for step in 0..10 {
            report(
                &breaker,
                id,
                at(start, step),
                OagwErrorKind::LinkUnavailable,
            );
        }
        assert_eq!(admit(&breaker, id, at(start, 10)), Admit::Dial);
    }

    #[test]
    fn a_zero_threshold_still_needs_a_failure() {
        let breaker = wrap(CircuitBreakerConfig {
            failure_threshold: 0,
            cooldown_secs: 30,
            ..CircuitBreakerConfig::default()
        });
        let id = uuid::Uuid::now_v7();
        let start = std::time::Instant::now();
        assert_eq!(admit(&breaker, id, start), Admit::Dial);
        answer(&breaker, id, start, 200);
        assert_eq!(admit(&breaker, id, at(start, 1)), Admit::Dial);
        report(&breaker, id, at(start, 1), OagwErrorKind::LinkUnavailable);
        assert_eq!(
            admit(&breaker, id, at(start, 2)),
            Admit::Refuse {
                retry_after_secs: 29
            },
            "the 29s of cooldown left is what the refusal names"
        );
    }

    #[test]
    fn the_states_the_operator_reads() {
        assert_eq!(State::Closed.label(), "closed");
        assert_eq!(State::Open.label(), "open");
        assert_eq!(State::HalfOpen.label(), "half_open");
        assert_eq!(State::Closed.value(), 0);
        assert_eq!(State::HalfOpen.value(), 1);
        assert_eq!(State::Open.value(), 2);
    }

    #[test]
    fn a_5xx_is_a_health_failure_and_a_4xx_is_not() {
        let server_error = |status: u16| {
            is_health_failure(Observed::Answered(
                http::StatusCode::from_u16(status).expect("a test status is a valid status"),
            ))
        };
        assert!(server_error(500));
        assert!(server_error(503));
        assert!(!server_error(404));
        assert!(!server_error(302), "a redirect is an answer, not a failure");
        assert!(is_health_failure(Observed::Failed(
            OagwErrorKind::LinkUnavailable
        )));
    }

    /// The exhaustive classification, kind by kind: the upstream's health is
    /// judged on these and on nothing else.
    #[test]
    fn every_kind_is_classified_once() {
        let failure_kinds = [
            OagwErrorKind::LinkUnavailable,
            OagwErrorKind::ConnectionTimeout,
            OagwErrorKind::RequestTimeout,
            OagwErrorKind::IdleTimeout,
            OagwErrorKind::StreamAborted,
            OagwErrorKind::DownstreamError,
            OagwErrorKind::ProtocolError,
        ];
        let client_kinds = [
            OagwErrorKind::Validation,
            OagwErrorKind::MissingTargetHost,
            OagwErrorKind::InvalidTargetHost,
            OagwErrorKind::UnknownTargetHost,
            OagwErrorKind::NotFound,
            OagwErrorKind::PayloadTooLarge,
            OagwErrorKind::RateLimitExceeded,
            OagwErrorKind::CorsOriginNotAllowed,
            OagwErrorKind::CorsMethodNotAllowed,
            OagwErrorKind::AliasConflict,
            OagwErrorKind::RouteConflict,
            OagwErrorKind::PluginConflict,
            OagwErrorKind::PluginInUse,
        ];
        let gateway_kinds = [
            OagwErrorKind::AuthenticationFailed,
            OagwErrorKind::SecretNotFound,
            OagwErrorKind::PluginNotFound,
            OagwErrorKind::Internal,
            OagwErrorKind::CircuitBreakerOpen,
        ];
        for kind in failure_kinds {
            assert!(
                is_health_failure(Observed::Failed(kind)),
                "{} counts",
                OagwError::new(kind, "the upstream").gts_type()
            );
        }
        for kind in client_kinds.iter().copied().chain(gateway_kinds) {
            assert!(
                !is_health_failure(Observed::Failed(kind)),
                "{} is not a health failure",
                OagwError::new(kind, "not the upstream's health").gts_type()
            );
        }
    }

    /// The problem type a refused request carries is the PRD's `503
    /// CircuitBreakerOpen`, retriable, whatever the breaker's own state is.
    #[test]
    fn the_refusal_is_the_documented_problem_type() {
        let kind = OagwErrorKind::CircuitBreakerOpen;
        assert_eq!(kind.status(), 503);
        assert_eq!(
            kind.gts_type(ResourceKind::Upstream),
            "gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1"
        );
    }

    /// A request that arrives behind the probe of a half-open breaker is
    /// refused, and the refusal names the probe budget still to run.
    #[test]
    fn a_request_behind_a_probe_is_refused_with_the_budget_still_to_run() {
        let breaker = breakers(1, BUDGET);
        let id = uuid::Uuid::now_v7();
        let start = std::time::Instant::now();
        report(&breaker, id, start, OagwErrorKind::LinkUnavailable);
        let Admit::Probe(_) = admit(&breaker, id, at(start, BUDGET)) else {
            panic!("the request past the cooldown is the probe");
        };
        assert_eq!(
            admit(&breaker, id, at(start, BUDGET + 10)),
            Admit::Refuse {
                retry_after_secs: 20
            },
            "the probe has been on the wire for 10s of its 30s budget"
        );
    }

    /// A probe that never reports is abandoned once its budget has run out, and
    /// the next request takes the slot — the safety valve that keeps a cancelled
    /// dial from refusing every later request forever.
    #[test]
    fn an_abandoned_probe_frees_its_slot_after_the_budget() {
        let breaker = breakers(1, BUDGET);
        let id = uuid::Uuid::now_v7();
        let start = std::time::Instant::now();
        report(&breaker, id, start, OagwErrorKind::LinkUnavailable);
        let Admit::Probe(abandoned) = admit(&breaker, id, at(start, BUDGET)) else {
            panic!("the request past the cooldown is the probe");
        };
        // One second before the budget ends the probe is still the probe, and
        // the refusal at the very end of it still names a second.
        assert_eq!(
            admit(&breaker, id, at(start, BUDGET * 2 - 1)),
            Admit::Refuse {
                retry_after_secs: 1
            }
        );
        assert_eq!(
            admit(&breaker, id, at(start, BUDGET * 2)),
            Admit::Refuse {
                retry_after_secs: 1
            },
            "exactly at the budget the refusal cannot name zero"
        );
        let Admit::Probe(replacement) = admit(&breaker, id, at(start, BUDGET * 2 + 1)) else {
            panic!("an abandoned probe has to free its slot");
        };
        // The abandoned probe reports late, and healthy: it is not the probe the
        // breaker is waiting for any more, so it must not close the breaker.
        let late = at(start, BUDGET * 2 + 2);
        let metrics = ProxyMetrics::from_global();
        breaker.record(
            id,
            "api.vendor.com",
            Some(abandoned),
            Observed::Answered(http::StatusCode::OK),
            late,
            &metrics,
        );
        assert_eq!(
            admit(&breaker, id, late),
            Admit::Refuse {
                retry_after_secs: 29
            },
            "the late report of an abandoned probe decided nothing"
        );
        // The replacement's own report is the one that closes the breaker.
        breaker.record(
            id,
            "api.vendor.com",
            Some(replacement),
            Observed::Answered(http::StatusCode::OK),
            late,
            &metrics,
        );
        assert_eq!(
            admit(&breaker, id, late),
            Admit::Dial,
            "the probe the breaker was waiting for closed it"
        );
    }

    /// A dial that was admitted while the breaker was closed can report long
    /// after the breaker tripped and half-opened: its outcome must not decide
    /// the state, because it is not the probe.
    #[test]
    fn a_late_report_from_a_closed_admission_decides_nothing() {
        let breaker = breakers(1, BUDGET);
        let id = uuid::Uuid::now_v7();
        let start = std::time::Instant::now();
        assert_eq!(admit(&breaker, id, start), Admit::Dial);
        // The slow request leaves. The request behind it fails and trips.
        report(&breaker, id, at(start, 1), OagwErrorKind::LinkUnavailable);
        let Admit::Probe(_) = admit(&breaker, id, at(start, 31)) else {
            panic!("the breaker half-opened and admitted its probe");
        };
        // The slow request now answers, 200, well after the trip.
        answer(&breaker, id, at(start, 32), 200);
        assert_eq!(
            admit(&breaker, id, at(start, 32)),
            Admit::Refuse {
                retry_after_secs: 29
            },
            "a request that was not the probe cannot close the breaker"
        );
    }

    /// A refusal always names a second, and never one that lets a client retry
    /// early: the budget still to run is rounded up to a whole second, so
    /// waiting it out always lands the client past the deadline.
    #[test]
    fn the_refusal_never_names_zero_seconds() {
        assert_eq!(retry_after(Duration::ZERO), 1);
        assert_eq!(
            retry_after(Duration::from_millis(1_500)),
            2,
            "rounded up to a whole second"
        );
        assert_eq!(
            retry_after(Duration::from_millis(29_999)),
            30,
            "the last millisecond is still a whole second to wait"
        );
        assert_eq!(
            retry_after(Duration::from_secs(30)),
            30,
            "a whole number of seconds is named as itself"
        );
    }
}
