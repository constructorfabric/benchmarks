//! The log sampling of the OAGW gear (entry 2.7).
//!
//! `cpt-cf-oagw-algo-log-sampling` fixes the sampling state of one line and
//! nothing else: the rate is the fixed 1-in-100 reading DESIGN §4.3 gives as
//! its example, it is not a configuration key of this feature or of the
//! entry-2.1 config set, and no per-route threshold exists and no route's
//! volume is measured.
//!
//! A failure, a circuit-breaker event and an authentication-failure line are
//! never sampled away; the authentication-failure class is rate-limited so a
//! flooding caller cannot grow the log volume without bound, and a line whose
//! class cannot be determined is emitted unsampled rather than dropped.

use parking_lot::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::audit::{AuditLevel, AuditRecord};

/// The fixed sampling rate of a successful `proxy_request` line: one line in
/// `SUCCESS_SAMPLE_RATE` consecutive successes.
pub const SUCCESS_SAMPLE_RATE: u64 = 100;

/// The GTS `type` identifier of an authentication failure, the class the
/// auth-failure rate limit is keyed on.
pub const AUTH_FAILURE_TYPE: &str = "cf.oagw.auth.failed.v1";

/// The GTS `type` identifier of an open circuit breaker, the class the
/// breaker-event sampling rule keys on.
pub const BREAKER_OPEN_TYPE: &str = "cf.oagw.circuit_breaker.open.v1";

/// The authentication-failure lines the rate limit admits per window.
pub const AUTH_FAILURE_BUDGET: u32 = 20;

/// The window the authentication-failure budget is measured over.
pub const AUTH_FAILURE_WINDOW: Duration = Duration::from_secs(1);

/// The event class a serialized line belongs to (`inst-ob-asamp-01`).
///
/// The class decides whether the line is sampled; it never decides a rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventClass {
    /// A successful `proxy_request` line, the class the fixed rate samples.
    Success,
    /// A `proxy_request` line whose outcome is a gateway failure.
    Failure,
    /// A circuit-breaker event, which is never sampled away.
    Breaker,
    /// A failed authentication attempt, which is rate-limited.
    AuthFailure,
    /// A line whose class cannot be determined, which is emitted unsampled.
    Unclassified,
}

/// The sampling decision a class leads to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// The line is handed to the writer.
    Emit,
    /// The line is dropped by the sampler and its drop is counted.
    Sampled,
}

/// The sampling state of the emission path.
///
/// The state is in-process and per gear process: it holds no per-request and no
/// per-route state, so the decision for one request is deterministic and holds
/// no memory of the previous one beyond the success counter and the
/// auth-failure window.
#[derive(Debug)]
pub struct Sampler {
    /// Consecutive successful lines classified so far.
    successes: AtomicU64,
    /// Authentication-failure lines the rate limit admitted.
    admitted: AtomicU64,
    /// Authentication-failure lines the rate limit dropped.
    throttled: AtomicU64,
    window: Mutex<Option<Window>>,
}

/// The open authentication-failure window.
#[derive(Debug)]
struct Window {
    started: Instant,
    admitted: u32,
}

impl Default for Sampler {
    fn default() -> Self {
        Self::new()
    }
}

impl Sampler {
    /// A sampler with an empty success counter and a closed window.
    #[must_use]
    pub fn new() -> Self {
        Self {
            successes: AtomicU64::new(0),
            admitted: AtomicU64::new(0),
            throttled: AtomicU64::new(0),
            window: Mutex::new(None),
        }
    }

    /// Classify a record (`inst-ob-asamp-01`).
    ///
    /// The class is read from the record itself, never from the request: no
    /// route, no path and no host takes part in the decision.
    #[must_use]
    pub fn classify(record: &AuditRecord) -> EventClass {
        if record.event != super::audit::AUDIT_EVENT {
            return EventClass::Unclassified;
        }
        if record.level == AuditLevel::Info && record.error_type.is_none() {
            return EventClass::Success;
        }
        match record.error_type.as_deref() {
            Some(error_type) if error_type.contains(BREAKER_OPEN_TYPE) => EventClass::Breaker,
            Some(error_type) if error_type.contains(AUTH_FAILURE_TYPE) => EventClass::AuthFailure,
            _ => EventClass::Failure,
        }
    }

    /// Decide what the class's line does (`inst-ob-asamp-02` to
    /// `inst-ob-asamp-04`).
    #[must_use]
    pub fn decide(&self, class: EventClass) -> Decision {
        // @cpt-begin:cpt-cf-oagw-algo-log-sampling:p1:inst-ob-asamp-02
        // The class decides whether the line is sampled away: a failure and a
        // circuit-breaker event are never sampled.
        match class {
            // @cpt-begin:cpt-cf-oagw-algo-log-sampling:p1:inst-ob-asamp-03
            // A failure, a circuit-breaker event and an authentication-failure
            // line are never sampled away; the authentication-failure class is
            // rate-limited instead, so a flooding caller cannot grow the log
            // volume without bound (DESIGN §4.3).
            EventClass::Failure | EventClass::Breaker => Decision::Emit,
            EventClass::AuthFailure => self.admit_auth_failure(),
            // @cpt-begin:cpt-cf-oagw-algo-log-sampling:p1:inst-ob-asamp-04
            // A line whose class cannot be determined is emitted unsampled
            // rather than dropped.
            EventClass::Unclassified => Decision::Emit,
            // @cpt-end:cpt-cf-oagw-algo-log-sampling:p1:inst-ob-asamp-04
            // @cpt-begin:cpt-cf-oagw-algo-log-sampling:p1:inst-ob-asamp-03a
            // @cpt-begin:cpt-cf-oagw-algo-log-sampling:p1:inst-ob-asamp-04a
            // A successful `proxy_request` line is sampled at the fixed rate of
            // 1 in 100, applied uniformly to every route with one in-process
            // counter, so no route's volume is measured, no route is named in
            // the decision and no configuration key is read to obtain the rate.
            EventClass::Success => self.sample_success(),
            // @cpt-end:cpt-cf-oagw-algo-log-sampling:p1:inst-ob-asamp-04a
            // @cpt-end:cpt-cf-oagw-algo-log-sampling:p1:inst-ob-asamp-03a
            // @cpt-end:cpt-cf-oagw-algo-log-sampling:p1:inst-ob-asamp-03
        }
        // @cpt-end:cpt-cf-oagw-algo-log-sampling:p1:inst-ob-asamp-02
    }

    /// The fixed 1-in-100 counter of the successful class
    /// (`inst-ob-asamp-03a`).
    ///
    /// Applied uniformly to every route with one in-process counter, so no
    /// route's volume is measured and no configuration key is read to obtain
    /// the rate.
    fn sample_success(&self) -> Decision {
        let seen = self.successes.fetch_add(1, Ordering::Relaxed) + 1;
        if seen.is_multiple_of(SUCCESS_SAMPLE_RATE) {
            Decision::Emit
        } else {
            Decision::Sampled
        }
    }

    /// The bounded emission of the authentication-failure class
    /// (`inst-ob-asamp-03`).
    ///
    /// The class is never sampled away, but it is rate-limited: at most
    /// [`AUTH_FAILURE_BUDGET`] lines per [`AUTH_FAILURE_WINDOW`], so a flooding
    /// caller cannot grow the log volume without bound. The window and its
    /// budget are in-process state, which resets with the gear (DECOMPOSITION
    /// assumption 3).
    fn admit_auth_failure(&self) -> Decision {
        let mut window = self.window.lock();
        let now = Instant::now();
        let expired = window
            .as_ref()
            .is_none_or(|open| open.started.elapsed() >= AUTH_FAILURE_WINDOW);
        if expired {
            *window = Some(Window {
                started: now,
                admitted: 0,
            });
        }
        let Some(open) = window.as_mut() else {
            return Decision::Emit;
        };
        if open.admitted >= AUTH_FAILURE_BUDGET {
            drop(window);
            self.throttled.fetch_add(1, Ordering::Relaxed);
            return Decision::Sampled;
        }
        open.admitted += 1;
        drop(window);
        self.admitted.fetch_add(1, Ordering::Relaxed);
        Decision::Emit
    }

    /// The lines the fixed rate sampled away.
    #[must_use]
    pub fn sampled(&self) -> u64 {
        self.success_seen()
            .saturating_sub(self.emitted_of_success())
    }

    /// The successful lines classified so far.
    #[must_use]
    pub fn success_seen(&self) -> u64 {
        self.successes.load(Ordering::Relaxed)
    }

    /// The successful lines the fixed rate admitted.
    #[must_use]
    pub fn emitted_of_success(&self) -> u64 {
        self.success_seen() / SUCCESS_SAMPLE_RATE
    }

    /// The authentication-failure lines the rate limit admitted.
    #[must_use]
    pub fn auth_admitted(&self) -> u64 {
        self.admitted.load(Ordering::Relaxed)
    }

    /// The authentication-failure lines the rate limit dropped.
    #[must_use]
    pub fn auth_throttled(&self) -> u64 {
        self.throttled.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::obs::audit::{AuditRecord, AUDIT_EVENT};

    fn record(level: AuditLevel, error_type: Option<&str>) -> AuditRecord {
        AuditRecord {
            timestamp: "2026-09-06T00:00:00.000Z".to_owned(),
            level,
            event: AUDIT_EVENT.to_owned(),
            request_id: None,
            tenant_id: None,
            principal_id: None,
            host: None,
            path: None,
            method: None,
            status: None,
            duration_ms: None,
            request_size: None,
            response_size: None,
            error_type: error_type.map(str::to_owned),
            error_message: None,
        }
    }

    #[test]
    fn a_successful_line_is_classified_as_a_success() {
        assert_eq!(
            Sampler::classify(&record(AuditLevel::Info, None)),
            EventClass::Success
        );
    }

    #[test]
    fn a_failure_is_classified_by_its_error_type() {
        let breaker = format!("{AUDIT_EVENT}:{BREAKER_OPEN_TYPE}");
        let auth = format!("{AUDIT_EVENT}:{AUTH_FAILURE_TYPE}");
        assert_eq!(
            Sampler::classify(&record(AuditLevel::Error, Some(&auth))),
            EventClass::AuthFailure
        );
        assert_eq!(
            Sampler::classify(&record(AuditLevel::Warn, Some(&breaker))),
            EventClass::Breaker
        );
        assert_eq!(
            Sampler::classify(&record(
                AuditLevel::Error,
                Some("cf.oagw.downstream.error.v1")
            )),
            EventClass::Failure
        );
        assert_eq!(
            Sampler::classify(&record(
                AuditLevel::Warn,
                Some("cf.oagw.rate_limit.exceeded.v1")
            )),
            EventClass::Failure
        );
    }

    #[test]
    fn an_unknown_event_is_unclassified_and_emitted_unsampled() {
        let mut unknown = record(AuditLevel::Info, None);
        unknown.event = "configuration_changed".to_owned();
        assert_eq!(Sampler::classify(&unknown), EventClass::Unclassified);
        assert_eq!(Sampler::new().decide(EventClass::Unclassified), Decision::Emit);
    }

    #[test]
    fn a_failure_line_is_never_sampled_away() {
        let sampler = Sampler::new();
        for _ in 0..1_000 {
            assert_eq!(sampler.decide(EventClass::Failure), Decision::Emit);
            assert_eq!(sampler.decide(EventClass::Breaker), Decision::Emit);
        }
        assert_eq!(sampler.success_seen(), 0, "no successful line was counted");
    }

    #[test]
    fn the_fixed_rate_is_one_in_a_hundred_and_uniform() {
        let sampler = Sampler::new();
        let mut emitted = 0;
        for _ in 0..1_000 {
            if sampler.decide(EventClass::Success) == Decision::Emit {
                emitted += 1;
            }
        }
        assert_eq!(sampler.success_seen(), 1_000);
        assert_eq!(emitted, 10, "exactly one line in a hundred is emitted");
        assert_eq!(sampler.emitted_of_success(), 10);
        assert_eq!(sampler.sampled(), 990);
    }

    #[test]
    fn the_hundredth_successful_line_is_emitted_and_the_next_are_not() {
        let sampler = Sampler::new();
        for _ in 0..SUCCESS_SAMPLE_RATE - 1 {
            assert_eq!(sampler.decide(EventClass::Success), Decision::Sampled);
        }
        assert_eq!(
            sampler.decide(EventClass::Success),
            Decision::Emit,
            "the hundredth success is emitted"
        );
        assert_eq!(
            sampler.decide(EventClass::Success),
            Decision::Sampled,
            "the counter starts over"
        );
    }

    #[test]
    fn the_sampling_rate_reads_no_configuration_and_names_no_route() {
        assert_eq!(SUCCESS_SAMPLE_RATE, 100);
        let sampler = Sampler::new();
        assert_eq!(
            sampler.decide(EventClass::Success),
            Decision::Sampled,
            "a success that is not the rate's is sampled away"
        );
        assert_eq!(
            sampler.decide(EventClass::Success),
            Decision::Sampled,
            "the decision is a counter, not a rate"
        );
        let mut lines = Vec::new();
        for _ in 0..1_000 {
            let mut line = record(AuditLevel::Info, None);
            line.path = Some("/oagw/v1/proxy/api.vendor.com/v1/things".to_owned());
            lines.push(Sampler::classify(&line));
        }
        assert!(lines.iter().all(|class| *class == EventClass::Success));
    }

    #[test]
    fn the_auth_failure_class_is_admitted_until_its_budget_is_spent() {
        let sampler = Sampler::new();
        for _ in 0..AUTH_FAILURE_BUDGET {
            assert_eq!(sampler.decide(EventClass::AuthFailure), Decision::Emit);
        }
        assert_eq!(sampler.auth_admitted(), u64::from(AUTH_FAILURE_BUDGET));
        for _ in 0..10 {
            assert_eq!(
                sampler.decide(EventClass::AuthFailure),
                Decision::Sampled,
                "the flooding caller cannot grow the log volume without bound"
            );
        }
        assert_eq!(sampler.auth_throttled(), 10);
        assert_eq!(sampler.auth_admitted(), u64::from(AUTH_FAILURE_BUDGET));
    }

    #[test]
    fn the_auth_failure_budget_is_restored_by_the_next_window() {
        let sampler = Sampler::new();
        for _ in 0..AUTH_FAILURE_BUDGET {
            let _ = sampler.decide(EventClass::AuthFailure);
        }
        assert_eq!(
            sampler.decide(EventClass::AuthFailure),
            Decision::Sampled,
            "the window is spent"
        );
        // Age the window past its measure, which is what a following window is.
        {
            let mut window = sampler.window.lock();
            *window = Some(Window {
                started: Instant::now() - AUTH_FAILURE_WINDOW,
                admitted: AUTH_FAILURE_BUDGET,
            });
        }
        assert_eq!(
            sampler.decide(EventClass::AuthFailure),
            Decision::Emit,
            "a new window admits again"
        );
    }

    #[test]
    fn the_sample_rate_is_a_constant_of_the_module_and_not_a_configuration_key() {
        // The rate is the fixed reading DESIGN §4.3 gives as its example: the
        // sampler takes no configuration input and reads no config set.
        assert_eq!(SUCCESS_SAMPLE_RATE, 100);
        let sampler = Sampler::new();
        let uniform = [
            EventClass::Success,
            EventClass::Failure,
            EventClass::Breaker,
            EventClass::AuthFailure,
            EventClass::Unclassified,
        ];
        for class in uniform {
            if class == EventClass::Success {
                continue;
            }
            let first = sampler.decide(class);
            let second = sampler.decide(class);
            assert_eq!(first, second, "{class:?} is decided by its class alone");
        }
    }
}
