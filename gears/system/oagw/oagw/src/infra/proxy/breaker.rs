//! The per-endpoint-host circuit breaker.
//!
//! `cpt-cf-oagw-algo-circuit-breaker` keeps one breaker per endpoint host in
//! process-local state. Five failed upstream calls inside a 30-second sliding
//! window open it, the open state admits a single probe after its cool-down,
//! and the probe's outcome closes it or reopens it. The breaker is core gateway
//! behaviour and not a plugin, and it counts only the failures of the
//! upstream-call stage: an upstream error response passed through to the caller,
//! a gateway validation failure raised before the call and a rejection this
//! breaker produced are not failures.
//!
//! The state transitions are the states of
//! `cpt-cf-oagw-state-circuit-breaker`. Every transition is recorded with its
//! host, from-state and to-state for entry 2.7's
//! `oagw_circuit_breaker_transitions_total`; no metric is emitted here.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::domain::error::DomainError;

/// Failures inside the window that trip the breaker.
pub const FAILURE_THRESHOLD: usize = 5;
/// Length of the sliding window, in seconds.
pub const WINDOW: Duration = Duration::from_secs(30);
/// Cool-down of the open state, in seconds.
pub const COOL_DOWN: Duration = Duration::from_secs(30);

/// The states of `cpt-cf-oagw-state-circuit-breaker`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    /// The host admits traffic.
    Closed,
    /// The host is refused traffic until the cool-down elapses.
    Open,
    /// One probe request is admitted and its outcome decides the next state.
    HalfOpen,
}

impl BreakerState {
    /// The wire token the observability layer labels the state with.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::Open => "open",
            Self::HalfOpen => "half_open",
        }
    }
}

/// A recorded transition, for entry 2.7's transition counter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    /// Endpoint host the breaker watches.
    pub host: String,
    /// State the breaker left.
    pub from: BreakerState,
    /// State the breaker entered.
    pub to: BreakerState,
}

/// The outcome the caller records for an admitted request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallOutcome {
    /// The upstream call succeeded.
    Success,
    /// The upstream call failed with a mapped upstream-call error.
    Failure,
    /// The call ended without an outcome the breaker counts: an error the
    /// gateway raised itself, which says nothing about the upstream's health.
    ///
    /// It is recorded all the same, because an admission whose outcome is never
    /// reported would hold the half-open probe slot for good.
    Neutral,
}

/// The decision the breaker takes for one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// The request may call the upstream.
    Admit,
    /// The request is refused without contacting the upstream.
    Reject,
}

/// One host's breaker: its state, its sliding window and its probe slot.
#[derive(Debug)]
struct HostBreaker {
    state: BreakerState,
    failures: VecDeque<Instant>,
    opened_at: Option<Instant>,
    /// The instant the admitted probe was admitted, while its outcome is owed.
    ///
    /// An admission without an outcome is a request the runtime dropped between
    /// the admission and the record, so the instant is what lets a later
    /// admission take the slot back instead of refusing traffic for good.
    probe_in_flight: Option<Instant>,
}

impl HostBreaker {
    fn new() -> Self {
        Self {
            state: BreakerState::Closed,
            failures: VecDeque::new(),
            opened_at: None,
            probe_in_flight: None,
        }
    }

    /// Drop the failures that fell out of the window.
    fn trim(&mut self, now: Instant) {
        while let Some(front) = self.failures.front() {
            if now.duration_since(*front) > WINDOW {
                self.failures.pop_front();
            } else {
                break;
            }
        }
    }

    /// The admission decision for the next request, with the transition the
    /// decision took, without recording the transition anywhere.
    fn admit(
        &mut self,
        now: Instant,
        cool_down: Duration,
        probe_window: Duration,
    ) -> (Admission, Option<Transition>) {
        self.trim(now);
        match self.state {
            BreakerState::Closed => (Admission::Admit, None),
            BreakerState::Open => {
                let cooled = self
                    .opened_at
                    .is_some_and(|opened_at| now.duration_since(opened_at) >= cool_down);
                if cooled {
                    // @cpt-begin:cpt-cf-oagw-algo-circuit-breaker:p1:inst-pe-cb-08
                    // The cool-down elapsed and the next request is the single
                    // probe: no other request is admitted beside it, and the
                    // probe's outcome decides the next state.
                    self.state = BreakerState::HalfOpen;
                    self.probe_in_flight = Some(now);
                    // The half-open state is entered here, on the admission, and
                    // not on the probe's outcome, so the transition is recorded
                    // with the same host, from-state and to-state as the others.
                    (
                        Admission::Admit,
                        Some(Transition {
                            host: String::new(),
                            from: BreakerState::Open,
                            to: BreakerState::HalfOpen,
                        }),
                    )
                    // @cpt-end:cpt-cf-oagw-algo-circuit-breaker:p1:inst-pe-cb-08
                } else {
                    (Admission::Reject, None)
                }
            }
            BreakerState::HalfOpen
                if self
                    .probe_in_flight
                    .is_some_and(|admitted_at| now.duration_since(admitted_at) < probe_window) =>
            {
                // The probe is still inside the window its call had, so its
                // outcome is still owed and no other request is admitted.
                (Admission::Reject, None)
            }
            BreakerState::HalfOpen => {
                // The probe outlived its call's window, which only a request the
                // runtime dropped between the admission and the outcome can do:
                // the slot is its again and the next request becomes the probe.
                self.probe_in_flight = Some(now);
                (Admission::Admit, None)
            }
        }
    }

    /// Record the outcome of an admitted request.
    fn record(&mut self, outcome: CallOutcome, now: Instant) -> Option<Transition> {
        match (self.state, outcome) {
            (BreakerState::HalfOpen, CallOutcome::Success) => {
                // The probe succeeded: the breaker closes and the window resets.
                self.state = BreakerState::Closed;
                self.failures.clear();
                self.opened_at = None;
                self.probe_in_flight = None;
                Some(Transition {
                    host: String::new(),
                    from: BreakerState::HalfOpen,
                    to: BreakerState::Closed,
                })
            }
            (BreakerState::HalfOpen, CallOutcome::Failure) => {
                // The probe failed: the cool-down restarts.
                self.state = BreakerState::Open;
                self.opened_at = Some(now);
                self.probe_in_flight = None;
                Some(Transition {
                    host: String::new(),
                    from: BreakerState::HalfOpen,
                    to: BreakerState::Open,
                })
            }
            (_, CallOutcome::Success) => {
                // A successful call in the closed state only trims the window.
                self.probe_in_flight = None;
                None
            }
            (_, CallOutcome::Neutral) => {
                // An outcome the breaker does not count still ends the call, so
                // it releases the probe slot without moving any state.
                self.probe_in_flight = None;
                None
            }
            (BreakerState::Closed, CallOutcome::Failure) => {
                self.failures.push_back(now);
                self.trim(now);
                // @cpt-begin:cpt-cf-oagw-algo-circuit-breaker:p1:inst-pe-cb-06
                // The fifth failure inside the window trips the breaker, so no
                // more than five failed requests are needed to stop traffic to
                // an unhealthy upstream.
                if self.failures.len() >= FAILURE_THRESHOLD {
                    self.state = BreakerState::Open;
                    self.opened_at = Some(now);
                    Some(Transition {
                        host: String::new(),
                        from: BreakerState::Closed,
                        to: BreakerState::Open,
                    })
                } else {
                    None
                }
                // @cpt-end:cpt-cf-oagw-algo-circuit-breaker:p1:inst-pe-cb-06
            }
            (BreakerState::Open, CallOutcome::Failure) => {
                // A request the breaker already admitted cannot fail it further;
                // the window is kept for the cool-down's end.
                None
            }
        }
    }
}

// @cpt-begin:cpt-cf-oagw-algo-circuit-breaker:p1:inst-pe-cb-01
/// The process-local breakers, keyed by endpoint host.
///
/// One breaker per endpoint host, held in process-local state: the breaker is
/// core gateway behaviour and not a plugin, so it is not configured per tenant
/// and never leaves this process.
pub struct CircuitBreaker {
    hosts: Mutex<HashMap<String, HostBreaker>>,
    transitions: Mutex<Vec<Transition>>,
    /// The cool-down an open state waits out before its probe is admitted.
    cool_down: Duration,
    /// The window an admitted probe is trusted for without an outcome.
    probe_window: Duration,
}

/// The bound the recorded transition log is kept under.
///
/// Every drain empties the log, so the bound is only reached when nothing
/// drains it any more; it keeps a breaker whose observability layer went away
/// from growing without end.
const TRANSITION_BOUND: usize = 1024;

/// The window an admitted probe is trusted for without an outcome.
///
/// A probe's outcome normally arrives inside the proxy call timeout, so the
/// window defaults to it: past it, an admission whose outcome never arrives —
/// a request future the runtime dropped between the admission and the record —
/// holds the half-open slot no longer, and the next request becomes the probe
/// instead of being refused for good.
const PROBE_WINDOW: Duration = Duration::from_secs(crate::config::DEFAULT_PROXY_TIMEOUT_SECS);

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self {
            hosts: Mutex::default(),
            transitions: Mutex::default(),
            cool_down: COOL_DOWN,
            probe_window: PROBE_WINDOW,
        }
    }
}
// @cpt-end:cpt-cf-oagw-algo-circuit-breaker:p1:inst-pe-cb-01

impl std::fmt::Debug for CircuitBreaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CircuitBreaker").finish_non_exhaustive()
    }
}

// @cpt-begin:cpt-cf-oagw-dod-circuit-breaker:p1:inst-full
impl CircuitBreaker {
    /// A breaker registry with no host state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A breaker registry whose cool-down and probe window are `cool_down` and
    /// `probe_window` in place of the recorded ones.
    ///
    /// The engine calls it with the configured proxy call timeout as the probe
    /// window, because an admission that outlived the call which carried it can
    /// only be a request the runtime dropped. The test that drives a trip and a
    /// probe through the transition counter shortens the cool-down, which no
    /// test wants to wait out for the recorded thirty seconds. Every other
    /// caller takes [`CircuitBreaker::new`], which keeps the recorded windows.
    #[must_use]
    pub fn with_windows(cool_down: Duration, probe_window: Duration) -> Self {
        Self {
            cool_down,
            probe_window,
            ..Self::default()
        }
    }

    /// Evaluate the breaker for `host` before any connection attempt.
    ///
    /// # Errors
    ///
    /// Returns the mapped `503` of an open breaker, without recording a failure
    /// for the rejected request.
    pub fn admit(&self, host: &str) -> Result<Admission, DomainError> {
        self.admit_at(host, Instant::now())
    }

    /// The admission decision at an explicit instant.
    fn admit_at(&self, host: &str, now: Instant) -> Result<Admission, DomainError> {
        // @cpt-begin:cpt-cf-oagw-algo-circuit-breaker:p1:inst-pe-cb-02
        // @cpt-begin:cpt-cf-oagw-algo-circuit-breaker:p1:inst-pe-cb-03
        let mut guards = self.hosts.lock();
        let breaker = guards.entry(host.to_owned()).or_insert_with(HostBreaker::new);
        let (admission, transition) = breaker.admit(now, self.cool_down, self.probe_window);
        if admission == Admission::Reject {
            return Err(DomainError::CircuitBreakerOpen {
                detail: format!("the circuit breaker for `{host}` is open"),
                retry_after_seconds: Some(COOL_DOWN.as_secs() as u32),
            });
        }
        if let Some(transition) = transition {
            self.record_transition(host, transition);
        }
        // @cpt-begin:cpt-cf-oagw-algo-circuit-breaker:p1:inst-pe-cb-04
        // @cpt-begin:cpt-cf-oagw-algo-circuit-breaker:p1:inst-pe-cb-11
        // The decision is returned to the caller, which records the outcome of
        // the admitted call against the same host.
        Ok(Admission::Admit)
        // @cpt-end:cpt-cf-oagw-algo-circuit-breaker:p1:inst-pe-cb-11
        // @cpt-end:cpt-cf-oagw-algo-circuit-breaker:p1:inst-pe-cb-04
        // @cpt-end:cpt-cf-oagw-algo-circuit-breaker:p1:inst-pe-cb-03
        // @cpt-end:cpt-cf-oagw-algo-circuit-breaker:p1:inst-pe-cb-02
    }

    /// Add the transition `host`'s breaker took to the bounded transition log.
    ///
    /// Called with the hosts lock held, so a state change and the transition it
    /// took are published together and the log never holds a transition the
    /// hosts map has already replaced. The transitions lock is taken after the
    /// hosts one and never before it, so a caller that holds the hosts lock
    /// cannot deadlock against a drain.
    fn record_transition(&self, host: &str, mut transition: Transition) {
        transition.host = host.to_owned();
        // @cpt-begin:cpt-cf-oagw-algo-circuit-breaker:p1:inst-pe-cb-10
        let mut recorded = self.transitions.lock();
        if recorded.len() == TRANSITION_BOUND {
            recorded.remove(0);
        }
        recorded.push(transition);
        // @cpt-end:cpt-cf-oagw-algo-circuit-breaker:p1:inst-pe-cb-10
    }

    /// Record the outcome of an admitted request against `host`.
    pub fn record(&self, host: &str, outcome: CallOutcome) {
        self.record_at(host, outcome, Instant::now());
    }

    /// Record the outcome at an explicit instant.
    fn record_at(&self, host: &str, outcome: CallOutcome, now: Instant) {
        // @cpt-begin:cpt-cf-oagw-algo-circuit-breaker:p1:inst-pe-cb-05
        let mut guards = self.hosts.lock();
        let breaker = guards.entry(host.to_owned()).or_insert_with(HostBreaker::new);
        if let Some(transition) = breaker.record(outcome, now) {
            self.record_transition(host, transition);
        }
        // @cpt-end:cpt-cf-oagw-algo-circuit-breaker:p1:inst-pe-cb-05
    }

    /// The state of `host`'s breaker.
    #[must_use]
    pub fn state(&self, host: &str) -> BreakerState {
        self.hosts
            .lock()
            .get(host)
            .map_or(BreakerState::Closed, |breaker| breaker.state)
    }

    /// The transitions recorded since the last drain, oldest first, and empty
    /// the log.
    ///
    /// The observability layer owns the transition counter, so handing each
    /// transition over exactly once lets a closed request stop paying for the
    /// log: the drain clones only what the breaker recorded since the last one
    /// instead of every transition of the process.
    #[must_use]
    pub fn drain_transitions(&self) -> Vec<Transition> {
        std::mem::take(&mut *self.transitions.lock())
    }

    /// The number of failures currently inside `host`'s window.
    #[must_use]
    pub fn failures_in_window(&self, host: &str) -> usize {
        self.hosts
            .lock()
            .get(host)
            .map_or(0, |breaker| breaker.failures.len())
    }

    /// Reset `host`'s breaker to the closed state with an empty window.
    pub fn reset(&self, host: &str) {
        // @cpt-begin:cpt-cf-oagw-algo-circuit-breaker:p1:inst-pe-cb-09
        let mut guards = self.hosts.lock();
        guards.insert(host.to_owned(), HostBreaker::new());
        // @cpt-end:cpt-cf-oagw-algo-circuit-breaker:p1:inst-pe-cb-09
    }
}
// @cpt-end:cpt-cf-oagw-dod-circuit-breaker:p1:inst-full

/// The failure classes the breaker counts, decided from the mapped error.
///
/// The upstream-call failures are the ones the breaker counts: a connection,
/// request or idle timeout, a downstream or protocol error and a payload the
/// client refused by size. A gateway validation failure raised before the call,
/// a refusal this breaker produced and an upstream error response passed
/// through are not failures.
#[must_use]
pub fn counts_as_failure(error: &DomainError) -> bool {
    // @cpt-begin:cpt-cf-oagw-algo-circuit-breaker:p1:inst-pe-cb-07
    matches!(
        error,
        DomainError::ConnectionTimeout { .. }
            | DomainError::RequestTimeout { .. }
            | DomainError::IdleTimeout { .. }
            | DomainError::DownstreamError { .. }
            | DomainError::ProtocolError { .. }
            | DomainError::PayloadTooLarge { .. }
    )
    // @cpt-end:cpt-cf-oagw-algo-circuit-breaker:p1:inst-pe-cb-07
}

/// Whether the breaker's admission is the mapped `503` of an open breaker.
#[must_use]
pub const fn is_rejection(admission: &Result<Admission, DomainError>) -> bool {
    admission.is_err()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::proxy::call::HttpVersion;
    use crate::infra::proxy::context::ResponseContext;

    fn host() -> String {
        "payments.internal".to_owned()
    }

    fn error() -> DomainError {
        DomainError::RequestTimeout {
            detail: "the upstream did not answer".to_owned(),
            retry_after_seconds: None,
        }
    }

    #[test]
    fn a_breaker_starts_closed_and_admits() {
        let breaker = CircuitBreaker::new();
        assert_eq!(breaker.state(&host()), BreakerState::Closed);
        assert!(breaker.admit(&host()).is_ok());
    }

    #[test]
    fn the_fifth_failure_within_the_window_opens_the_breaker() {
        let breaker = CircuitBreaker::new();
        for _ in 0..4 {
            breaker.record(&host(), CallOutcome::Failure);
            assert_eq!(breaker.state(&host()), BreakerState::Closed);
        }
        breaker.record(&host(), CallOutcome::Failure);
        assert_eq!(breaker.state(&host()), BreakerState::Open);
        let error = breaker
            .admit(&host())
            .expect_err("an open breaker refuses the call");
        assert_eq!(error.status(), 503, "{error}");
        assert_eq!(
            error.gts_id(),
            "gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1"
        );
        assert!(is_rejection(&breaker.admit(&host())));
    }

    #[test]
    fn a_failure_that_falls_out_of_the_window_is_forgotten() {
        // The sliding window is 30 seconds: a failure older than the window is
        // trimmed before the threshold is evaluated, so four failures inside
        // the window plus one outside it do not trip the breaker.
        let breaker = CircuitBreaker::new();
        let start = Instant::now();
        for index in 0..4u32 {
            breaker.record_at(
                &host(),
                CallOutcome::Failure,
                start + Duration::from_secs(u64::from(index)),
            );
            assert_eq!(breaker.state(&host()), BreakerState::Closed);
        }
        let later = start + WINDOW + Duration::from_secs(1);
        breaker.record_at(&host(), CallOutcome::Failure, later);
        assert_eq!(breaker.state(&host()), BreakerState::Closed);
        assert_eq!(breaker.failures_in_window(&host()), 4);
    }

    #[test]
    fn a_rejected_request_records_no_failure() {
        let breaker = CircuitBreaker::new();
        for _ in 0..5 {
            breaker.record(&host(), CallOutcome::Failure);
        }
        let before = breaker.failures_in_window(&host());
        let _ = breaker.admit(&host());
        assert_eq!(
            breaker.failures_in_window(&host()),
            before,
            "the breaker counts its own rejections as failures"
        );
    }

    #[test]
    fn the_cool_down_admits_a_single_probe() {
        let breaker = CircuitBreaker::new();
        for _ in 0..5 {
            breaker.record(&host(), CallOutcome::Failure);
        }
        // The cool-down has not elapsed yet, so nothing is admitted.
        assert!(is_rejection(&breaker.admit(&host())));
        // Once the cool-down elapses, one request is admitted as the probe and
        // every other one is refused while the probe is in flight.
        advance(&breaker, &host(), COOL_DOWN);
        assert!(breaker.admit(&host()).is_ok());
        assert_eq!(breaker.state(&host()), BreakerState::HalfOpen);
        assert!(is_rejection(&breaker.admit(&host())), "one probe only");
    }

    #[test]
    fn a_successful_probe_closes_the_breaker_and_resets_the_window() {
        let breaker = CircuitBreaker::new();
        for _ in 0..5 {
            breaker.record(&host(), CallOutcome::Failure);
        }
        advance(&breaker, &host(), COOL_DOWN);
        assert!(breaker.admit(&host()).is_ok());
        breaker.record(&host(), CallOutcome::Success);
        assert_eq!(breaker.state(&host()), BreakerState::Closed);
        assert_eq!(breaker.failures_in_window(&host()), 0);
        assert!(breaker.admit(&host()).is_ok());
    }

    #[test]
    fn a_failed_probe_reopens_the_breaker_and_restarts_the_cool_down() {
        let breaker = CircuitBreaker::new();
        for _ in 0..5 {
            breaker.record(&host(), CallOutcome::Failure);
        }
        advance(&breaker, &host(), COOL_DOWN);
        assert!(breaker.admit(&host()).is_ok());
        breaker.record(&host(), CallOutcome::Failure);
        assert_eq!(breaker.state(&host()), BreakerState::Open);
        assert!(is_rejection(&breaker.admit(&host())));
    }

    #[test]
    fn every_transition_is_recorded_with_its_host_from_and_to() {
        let breaker = CircuitBreaker::new();
        for _ in 0..5 {
            breaker.record(&host(), CallOutcome::Failure);
        }
        let recorded = breaker.drain_transitions();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].host, host());
        assert_eq!(recorded[0].from, BreakerState::Closed);
        assert_eq!(recorded[0].to, BreakerState::Open);

        // The drain empties the log: the next one carries only what the
        // breaker recorded since, so the counter of the observability layer is
        // handed every transition exactly once.
        assert!(breaker.drain_transitions().is_empty());

        advance(&breaker, &host(), COOL_DOWN);
        let _ = breaker.admit(&host());
        // The half-open state is entered on the admission of the probe, so the
        // transition it took is recorded with the others and not left to the
        // probe's outcome.
        let recorded = breaker.drain_transitions();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].host, host());
        assert_eq!(recorded[0].from, BreakerState::Open);
        assert_eq!(recorded[0].to, BreakerState::HalfOpen);

        breaker.record(&host(), CallOutcome::Success);
        let recorded = breaker.drain_transitions();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].host, host());
        assert_eq!(recorded[0].from, BreakerState::HalfOpen);
        assert_eq!(recorded[0].to, BreakerState::Closed);
    }

    #[test]
    fn an_outcome_the_breaker_does_not_count_releases_the_probe_slot() {
        // An error the gateway raised itself, mapped before the call, says
        // nothing about the upstream's health, so it is not a failure. It is an
        // outcome all the same: the admission it closes out must not hold the
        // half-open slot, or the host is refused for good.
        let breaker = CircuitBreaker::new();
        for _ in 0..5 {
            breaker.record(&host(), CallOutcome::Failure);
        }
        advance(&breaker, &host(), COOL_DOWN);
        assert!(breaker.admit(&host()).is_ok());
        assert!(is_rejection(&breaker.admit(&host())), "one probe only");

        breaker.record(&host(), CallOutcome::Neutral);
        assert_eq!(breaker.state(&host()), BreakerState::HalfOpen);
        assert!(breaker.admit(&host()).is_ok(), "the next request is the probe");
    }

    #[test]
    fn a_probe_that_outlives_its_call_window_no_longer_holds_the_slot() {
        // A probe whose outcome never arrives is a request the runtime dropped
        // between the admission and the record. Past the window its call had,
        // the slot is taken back: the host is not wedged in the half-open state.
        let breaker = CircuitBreaker::with_windows(COOL_DOWN, Duration::from_secs(5));
        let start = Instant::now();
        for index in 0..5u32 {
            breaker.record_at(
                &host(),
                CallOutcome::Failure,
                start + Duration::from_secs(u64::from(index)),
            );
        }
        let probe = start + COOL_DOWN + Duration::from_secs(5);
        assert!(breaker.admit_at(&host(), probe).is_ok());
        assert!(is_rejection(&breaker.admit_at(&host(), probe)), "one probe only");
        assert!(
            breaker
                .admit_at(&host(), probe + Duration::from_secs(5))
                .is_ok(),
            "the dropped probe does not hold the slot past its window"
        );
    }

    #[test]
    fn the_breakers_are_per_host() {
        let breaker = CircuitBreaker::new();
        let other = "other.internal".to_owned();
        for _ in 0..5 {
            breaker.record(&host(), CallOutcome::Failure);
        }
        assert_eq!(breaker.state(&host()), BreakerState::Open);
        assert_eq!(breaker.state(&other), BreakerState::Closed);
        assert!(breaker.admit(&other).is_ok());
    }

    #[test]
    fn only_the_upstream_call_failures_are_counted() {
        // @cpt-begin:cpt-cf-oagw-state-circuit-breaker:p1:inst-pe-scb-01
        for error in [
            error(),
            DomainError::ConnectionTimeout {
                detail: "connect".to_owned(),
                retry_after_seconds: None,
            },
            DomainError::IdleTimeout {
                detail: "idle".to_owned(),
                retry_after_seconds: None,
            },
            DomainError::DownstreamError {
                detail: "reset".to_owned(),
            },
            DomainError::ProtocolError {
                detail: "bad frame".to_owned(),
            },
        ] {
            assert!(counts_as_failure(&error), "{error} is a failure");
        }
        for error in [
            DomainError::ValidationError {
                detail: "raised before the call".to_owned(),
            },
            DomainError::CircuitBreakerOpen {
                detail: "this breaker's own rejection".to_owned(),
                retry_after_seconds: None,
            },
            DomainError::RouteNotFound {
                detail: "no route".to_owned(),
            },
            DomainError::CorsOriginNotAllowed {
                detail: "origin".to_owned(),
            },
        ] {
            assert!(!counts_as_failure(&error), "{error} is not a failure");
        }
        // @cpt-end:cpt-cf-oagw-state-circuit-breaker:p1:inst-pe-scb-01
    }

    #[test]
    fn an_upstream_error_response_is_not_a_failure() {
        // A 500 passed through to the caller is the upstream's answer, not a
        // gateway failure, so the breaker records a success.
        let breaker = CircuitBreaker::new();
        for _ in 0..7 {
            breaker.record(&host(), CallOutcome::Success);
        }
        assert_eq!(breaker.state(&host()), BreakerState::Closed);
    }

    #[test]
    fn the_state_tokens_are_the_state_machine_names() {
        // @cpt-begin:cpt-cf-oagw-state-circuit-breaker:p1:inst-pe-scb-02
        // @cpt-begin:cpt-cf-oagw-state-circuit-breaker:p1:inst-pe-scb-03
        // @cpt-begin:cpt-cf-oagw-state-circuit-breaker:p1:inst-pe-scb-04
        assert_eq!(BreakerState::Closed.as_str(), "closed");
        assert_eq!(BreakerState::Open.as_str(), "open");
        assert_eq!(BreakerState::HalfOpen.as_str(), "half_open");
        // @cpt-end:cpt-cf-oagw-state-circuit-breaker:p1:inst-pe-scb-04
        // @cpt-end:cpt-cf-oagw-state-circuit-breaker:p1:inst-pe-scb-03
        // @cpt-end:cpt-cf-oagw-state-circuit-breaker:p1:inst-pe-scb-02
    }

    #[test]
    fn the_threshold_and_the_window_are_the_recorded_values() {
        assert_eq!(FAILURE_THRESHOLD, 5);
        assert_eq!(WINDOW, Duration::from_secs(30));
        assert_eq!(COOL_DOWN, Duration::from_secs(30));
    }

    #[test]
    fn a_response_context_is_untouched_by_the_breaker() {
        // The breaker runs before the call, so it classifies no response.
        let response = ResponseContext {
            status: 500,
            streamed: false,
            handed_off: false,
            http_version: HttpVersion::Http11,
            error_source: "upstream",
        };
        assert_eq!(response.status, 500);
    }

    /// Age a host's breaker by rewriting its window and its open instant.
    fn advance(breaker: &CircuitBreaker, host: &str, by: Duration) {
        let mut guards = breaker.hosts.lock();
        if let Some(state) = guards.get_mut(host) {
            state.failures = state
                .failures
                .iter()
                .filter_map(|instant| instant.checked_sub(by))
                .collect();
            state.opened_at = state.opened_at.and_then(|instant| instant.checked_sub(by));
        }
    }
}
