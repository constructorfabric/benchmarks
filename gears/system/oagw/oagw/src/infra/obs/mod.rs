//! Observability surface of the OAGW gear (entry 2.7).
//!
//! Feature 1 (Gear Foundation) emits structured startup log lines only and
//! registers no metric family and writes no audit record: OAGW registers metric
//! families on the host-provided metrics surface (DECOMPOSITION assumption 9)
//! and audit logging is entry 2.7.
//!
//! The entry-2.7 surface is one [`Observability`] instance per gear process,
//! which holds the metric registry, the bounded non-blocking writer and the
//! sampler, and turns one closed request context, one in-flight change or one
//! breaker transition into an emission that never blocks the request path and
//! never fails a request.

pub mod audit;
pub mod correlation;
pub mod metrics;
pub mod sampling;
pub mod writer;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use crate::domain::error::DomainError;
use crate::infra::proxy::breaker::BreakerState;
use crate::infra::proxy::context::RequestContext;
use crate::infra::proxy::engine::ProxyEngine;

pub use audit::{
    AuditInput, AuditLevel, AuditRecord, StreamFold, AUDIT_EVENT, AUDIT_KEYS, KEY_ERROR_MESSAGE,
};
pub use correlation::{correlation_id, CORRELATION_HEADER};
pub use metrics::{
    FamilyDescriptor, InFlightGuard, MetricKind, MetricRegistry, RegistrationFailure, RegistryState,
    RuntimeState, DURATION_BUCKETS, FAMILIES,
};
pub use sampling::{Decision, EventClass, Sampler, SUCCESS_SAMPLE_RATE};
pub use writer::{AuditWriter, WRITER_CAPACITY};

/// The outcome of one closed request the emission reads.
///
/// It is the input of both the metric families and the audit line, so one
/// closed outcome produces one metric update set and exactly one line.
#[derive(Debug, Clone, Copy)]
pub struct RequestObservation<'a> {
    /// The closed request context.
    pub context: &'a RequestContext,
    /// The numeric status the client received.
    pub status: u16,
    /// Whether the pipeline made an upstream call for the request.
    pub upstream_called: bool,
    /// The request's own start-to-close interval.
    pub duration: Duration,
    /// Bytes the client received, when the exchange produced a counted body.
    pub response_bytes: Option<u64>,
    /// The tenant the request was authenticated for.
    pub tenant_id: Option<&'a str>,
    /// The principal the request was authenticated as.
    pub principal_id: Option<&'a str>,
    /// The failure the gateway raised, when the outcome is one.
    pub error: Option<&'a DomainError>,
}

impl RequestObservation<'_> {
    /// The GTS `type` identifier the request families and the audit line carry.
    #[must_use]
    pub fn error_type(&self) -> Option<&'static str> {
        self.error
            .map_or(self.context.error_type, |error| Some(error.gts_id()))
    }
}

/// The live state the observable gauges read, taken from the proxy engine.
///
/// The breaker of `cpt-cf-oagw-state-circuit-breaker` is the pipeline's state,
/// so the collectors read it through this adapter and the registry never holds
/// a copy of it. The engine is held weakly: the adapter cannot keep the gear's
/// engine alive after the gear dropped it.
pub struct BreakerSource {
    engine: Weak<ProxyEngine>,
}

impl BreakerSource {
    /// The source of the breaker state one engine holds.
    #[must_use]
    pub fn new(engine: &Arc<ProxyEngine>) -> Self {
        Self {
            engine: Arc::downgrade(engine),
        }
    }
}

impl RuntimeState for BreakerSource {
    fn breaker_state(&self, host: &str) -> BreakerState {
        let Some(engine) = self.engine.upgrade() else {
            return BreakerState::Closed;
        };
        engine.breaker().state(host)
    }

    fn breaker_drain_transitions(&self) -> Vec<crate::infra::proxy::breaker::Transition> {
        let Some(engine) = self.engine.upgrade() else {
            return Vec::new();
        };
        engine.breaker().drain_transitions()
    }
}

/// The observability layer of the gear.
///
/// One instance per gear process, held for the lifetime of the process: the
/// metric series, the in-flight counters, the sampling counter and the writer's
/// bound channel are in-process state and are dropped with it, so the next
/// start begins from empty series (DECOMPOSITION assumption 3).
pub struct Observability {
    metrics: Arc<MetricRegistry>,
    writer: Arc<AuditWriter>,
    sampler: Sampler,
    registered: AtomicBool,
    dropped_lines: AtomicU64,
}

impl std::fmt::Debug for Observability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Observability")
            .field("metrics", &self.metrics)
            .field("writer", &self.writer)
            .field("registered", &self.registered())
            .field("dropped_lines", &self.dropped_lines())
            .finish_non_exhaustive()
    }
}

/// The shared observability layer of the process.
static SHARED: std::sync::OnceLock<Arc<Observability>> = std::sync::OnceLock::new();

impl Observability {
    /// A layer whose registry is `unregistered` and whose writer is idle.
    #[must_use]
    pub fn new() -> Self {
        Self {
            metrics: Arc::new(MetricRegistry::new()),
            writer: Arc::new(AuditWriter::new()),
            sampler: Sampler::new(),
            registered: AtomicBool::new(false),
            dropped_lines: AtomicU64::new(0),
        }
    }

    /// The layer the gear's transports share.
    ///
    /// The families are registered on first use, so a process that never
    /// initializes a transport never registers them; the registration is
    /// idempotent and a failure leaves the layer serving traffic with no metric
    /// emission and no audit line lost to it.
    #[must_use]
    pub fn shared() -> Arc<Self> {
        Arc::clone(SHARED.get_or_init(|| {
            let layer = Arc::new(Self::new());
            layer.register();
            layer
        }))
    }

    /// The metric registry the families are recorded on.
    #[must_use]
    pub const fn metrics(&self) -> &Arc<MetricRegistry> {
        &self.metrics
    }

    /// The bounded non-blocking writer the audit lines are offered to.
    #[must_use]
    pub const fn writer(&self) -> &Arc<AuditWriter> {
        &self.writer
    }

    /// Whether the metric families are registered.
    #[must_use]
    pub fn registered(&self) -> bool {
        self.registered.load(Ordering::Acquire)
    }

    /// The lines the emission dropped instead of writing.
    #[must_use]
    pub fn dropped_lines(&self) -> u64 {
        self.dropped_lines.load(Ordering::Relaxed)
    }

    /// Register the metric families of DESIGN §4.2
    /// (`cpt-cf-oagw-state-metric-registry`).
    ///
    /// A family the host surface rejects leaves no partial family behind: the
    /// failure is logged at `ERROR`, the gear serves traffic with no metric
    /// emission, and a request is never failed because of it.
    ///
    /// A fresh registration also restores the receiving half of the audit
    /// channel, because the start that follows a stop in this process has to
    /// drain again: the previous one's drain took the half with it when the
    /// gear returned. The lines the previous channel still held were written
    /// out by that stop's flush window, so the restored pair begins empty.
    pub fn register(&self) {
        // @cpt-begin:cpt-cf-oagw-flow-metrics-observation:p1:inst-ob-met-01
        // The actor's scrape arrives on the host-provided metrics surface: the
        // gear registers the collectors the host serves and runs no scrape path
        // of its own.
        // @cpt-end:cpt-cf-oagw-flow-metrics-observation:p1:inst-ob-met-01
        // @cpt-begin:cpt-cf-oagw-flow-metrics-observation:p1:inst-ob-met-02
        // The host authenticates, authorizes and serves the endpoint; this gear
        // registers no endpoint of its own and answers no scrape itself
        // (DECOMPOSITION assumption 9).
        // @cpt-end:cpt-cf-oagw-flow-metrics-observation:p1:inst-ob-met-02
        // @cpt-begin:cpt-cf-oagw-flow-metrics-observation:p1:inst-ob-met-03
        // The families the gear registers at initialization are the twelve of
        // DESIGN §4.2, collected into the exposition document the host serves.
        if self.metrics.register().is_ok() {
            self.registered.store(true, Ordering::Release);
            self.writer.restore_receiver();
        }
        // @cpt-end:cpt-cf-oagw-flow-metrics-observation:p1:inst-ob-met-03
    }

    /// Tear the layer down with the gear
    /// (`cpt-cf-oagw-state-metric-registry` st-08/st-09).
    ///
    /// The in-memory collectors are emptied and the families go back to
    /// `unregistered`, so a gear restarted in the same process begins from
    /// empty series. The call is idempotent and a layer that was never
    /// registered tears down to the same state.
    pub fn teardown(&self) {
        // @cpt-begin:cpt-cf-oagw-state-metric-registry:p1:inst-ob-mreg-03
        // `registered` -> `unregistered`: the host process tears the gear down,
        // and the in-memory counters and gauges are dropped with it, so the
        // next start begins from empty series (DECOMPOSITION assumption 3).
        self.registered.store(false, Ordering::Release);
        self.metrics.teardown();
        // @cpt-end:cpt-cf-oagw-state-metric-registry:p1:inst-ob-mreg-03
    }

    /// Attach the live state the observable gauges read to the registry.
    ///
    /// The source is held by the registry and read at scrape time only; the
    /// engine behind it is held weakly, so the adapter never keeps the gear's
    /// engine alive after the gear dropped it.
    pub fn attach_state_source(&self, source: Arc<dyn RuntimeState>) {
        self.metrics.attach_state_source(source);
    }

    /// Spawn the drain task the audit lines are written by.
    ///
    /// Called once per start at the gear's initialization; the returned handle
    /// is awaited by the gear's `run` `select!`, so the drain is joined before
    /// the gear returns. A start that follows a stop drains again, because the
    /// registration that start made restored the receiving half.
    pub fn spawn_drain(&self, token: CancellationToken) -> Option<tokio::task::JoinHandle<()>> {
        let receiver = self.writer.take_receiver()?;
        let writer = Arc::clone(&self.writer);
        Some(tokio::spawn(writer::drain(receiver, writer, token)))
    }

    /// Record the outcome of one closed request
    /// (`cpt-cf-oagw-flow-request-audit`).
    ///
    /// This is the one entry point the transport's close of a proxied request
    /// calls: it updates the request families, builds the audit line of the
    /// closed outcome and offers it to the writer without blocking. No failure
    /// of any of those three steps reaches the caller.
    pub fn record_request(&self, observation: RequestObservation<'_>) {
        // @cpt-begin:cpt-cf-oagw-flow-request-audit:p1:inst-ob-audit-07a
        // The same closed request outcome is handed to the metric emit, so the
        // request families and the audit line describe the same request.
        self.metrics.record_request(&observation);
        // @cpt-end:cpt-cf-oagw-flow-request-audit:p1:inst-ob-audit-07a

        let record = AuditRecord::build(&AuditInput {
            context: observation.context,
            status: observation.status,
            duration: observation.duration,
            response_bytes: observation.response_bytes,
            error: observation.error,
            tenant_id: observation.tenant_id,
            principal_id: observation.principal_id,
            stream: observation
                .context
                .stream
                .as_ref()
                .and_then(|stream| stream.outcome())
                .as_ref()
                .map(StreamFold::of),
        });
        self.emit(&record);
    }

    /// Sample and offer one built line
    /// (`cpt-cf-oagw-algo-log-sampling`).
    pub fn emit(&self, record: &AuditRecord) {
        // @cpt-begin:cpt-cf-oagw-flow-request-audit:p1:inst-ob-audit-08
        // @cpt-begin:cpt-cf-oagw-algo-log-sampling:p1:inst-ob-asamp-01
        // The class the record belongs to decides whether the line is sampled;
        // the class never decides a rate, which is fixed.
        let class = Sampler::classify(record);
        if self.sampler.decide(class) == Decision::Sampled {
            return;
        }
        // @cpt-end:cpt-cf-oagw-algo-log-sampling:p1:inst-ob-asamp-01
        // @cpt-end:cpt-cf-oagw-flow-request-audit:p1:inst-ob-audit-08

        // @cpt-begin:cpt-cf-oagw-algo-audit-line:p1:inst-ob-aline-09
        // @cpt-begin:cpt-cf-oagw-algo-audit-line:p1:inst-ob-aline-10
        // The document is serialized as a single JSON line and offered to the
        // bounded writer without blocking.
        match record.to_line() {
            Some(line) => {
                self.writer.offer(line);
            }
            None => {
                // @cpt-begin:cpt-cf-oagw-algo-audit-line:p1:inst-ob-aline-11
                // @cpt-begin:cpt-cf-oagw-algo-audit-line:p1:inst-ob-aline-12
                // @cpt-begin:cpt-cf-oagw-flow-request-audit:p1:inst-ob-audit-09
                // @cpt-begin:cpt-cf-oagw-flow-request-audit:p1:inst-ob-audit-10
                // A serialization failure drops the line, counts the drop
                // in-process and raises no error to the caller: the request's
                // response is already committed and is never retried, altered
                // or delayed by an emission failure.
                self.dropped_lines.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(
                    dropped_lines = self.dropped_lines(),
                    "an audit line could not be serialized and was dropped"
                );
                // @cpt-end:cpt-cf-oagw-flow-request-audit:p1:inst-ob-audit-10
                // @cpt-end:cpt-cf-oagw-flow-request-audit:p1:inst-ob-audit-09
                // @cpt-end:cpt-cf-oagw-algo-audit-line:p1:inst-ob-aline-12
                // @cpt-end:cpt-cf-oagw-algo-audit-line:p1:inst-ob-aline-11
            }
        }
        // @cpt-end:cpt-cf-oagw-algo-audit-line:p1:inst-ob-aline-10
        // @cpt-end:cpt-cf-oagw-algo-audit-line:p1:inst-ob-aline-09
        // @cpt-begin:cpt-cf-oagw-algo-audit-line:p1:inst-ob-aline-13
        // The emission result is returned: the line was offered to the writer or
        // was dropped and counted, and the caller receives no error either way.
        // @cpt-end:cpt-cf-oagw-algo-audit-line:p1:inst-ob-aline-13
    }

    /// Record the in-flight change one open request context is
    /// (`cpt-cf-oagw-flow-runtime-state-observation`).
    #[must_use]
    pub fn request_opened(&self) -> InFlightGuard {
        self.metrics.request_opened()
    }

    /// Record one upstream-call duration observation (`upstream` phase).
    pub fn observe_upstream_duration(&self, host: &str, route: &str, duration: Duration) {
        self.metrics.observe_upstream_duration(host, route, duration);
    }

    /// The sampler, for the tests that hold the sampling arithmetic to account.
    #[must_use]
    pub const fn sampler(&self) -> &Sampler {
        &self.sampler
    }
}

impl Default for Observability {
    fn default() -> Self {
        Self::new()
    }
}

/// The instant a request's own start-to-close interval is measured from.
#[must_use]
pub fn request_started() -> Instant {
    Instant::now()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::proxy::breaker::Transition;
    use crate::infra::proxy::context::RequestContext;
    use std::sync::Mutex;
    use std::time::Duration;

    fn layer() -> Arc<Observability> {
        let layer = Arc::new(Observability::new());
        layer.register();
        layer
    }

    fn context(alias: Option<&str>) -> RequestContext {
        let mut context = RequestContext::new(
            "corr-1".to_owned(),
            "/oagw/v1/proxy/api.vendor.com/v1/things?q=1".to_owned(),
            "GET".to_owned(),
        );
        context.alias = alias.map(str::to_owned);
        context.matched_route = Some("/v1/things".to_owned());
        context.upstream_id = Some("up-1".to_owned());
        context.endpoint_host = Some("10.0.0.1".to_owned());
        context.selection = Some(crate::infra::proxy::context::SelectionMethod::Default);
        context.request_bytes = Some(12);
        context
    }

    /// A live state the observable gauges read, without an engine behind it.
    struct StubState {
        states: Vec<(&'static str, BreakerState)>,
        transitions: Mutex<Vec<Transition>>,
    }

    impl RuntimeState for StubState {
        fn breaker_state(&self, host: &str) -> BreakerState {
            self.states
                .iter()
                .find(|(recorded, _)| *recorded == host)
                .map_or(BreakerState::Closed, |(_, state)| *state)
        }

        fn breaker_drain_transitions(&self) -> Vec<Transition> {
            std::mem::take(&mut self.transitions.lock().unwrap())
        }
    }

    fn transition(host: &str, from: BreakerState, to: BreakerState) -> Transition {
        Transition {
            host: host.to_owned(),
            from,
            to,
        }
    }

    #[test]
    fn one_closed_request_produces_one_metric_update_set_and_one_line_offer() {
        let layer = layer();
        let _ = layer.writer().enable_capture();
        let context = context(Some("api.vendor.com"));
        layer.record_request(RequestObservation {
            context: &context,
            status: 200,
            upstream_called: true,
            duration: Duration::from_millis(3),
            response_bytes: Some(48),
            tenant_id: Some("tenant"),
            principal_id: Some("principal"),
            error: None,
        });
        let registry = layer.metrics();
        assert!(
            registry
                .counter(
                    metrics::REQUESTS_TOTAL,
                    &[
                        ("host", "api.vendor.com"),
                        ("http.request.method", "GET"),
                        ("http.route", "/v1/things"),
                        ("http.response.status_code", "200"),
                    ],
                )
                .is_some(),
            "the request counter recorded the closed request"
        );
        assert!(
            registry
                .histogram(
                    metrics::REQUEST_DURATION,
                    &[
                        ("host", "api.vendor.com"),
                        ("http.route", "/v1/things"),
                        ("phase", "gateway_added"),
                    ],
                )
                .is_some(),
            "the duration histogram observed the closed request"
        );
        assert!(
            registry
                .counter(
                    metrics::ROUTING_ENDPOINT_SELECTED,
                    &[
                        ("upstream_id", "up-1"),
                        ("endpoint_host", "10.0.0.1"),
                        ("selection_method", "default"),
                    ],
                )
                .is_some()
        );
        assert!(layer.writer().written() == 0, "the drain is not running here");
    }

    #[test]
    fn a_layer_registers_its_families_once_and_is_idempotent_after_that() {
        let layer = layer();
        assert!(layer.registered());
        layer.register();
        assert!(layer.registered());
        assert_eq!(layer.metrics().families().len(), FAMILIES.len());
    }

    #[test]
    fn an_unregistered_layer_records_no_series_and_drops_no_request() {
        let layer = Arc::new(Observability::new());
        let context = context(None);
        layer.record_request(RequestObservation {
            context: &context,
            status: 503,
            upstream_called: false,
            duration: Duration::from_millis(1),
            response_bytes: None,
            tenant_id: None,
            principal_id: None,
            error: Some(&DomainError::CircuitBreakerOpen {
                detail: "open".to_owned(),
                retry_after_seconds: None,
            }),
        });
        assert!(!layer.registered());
        assert!(
            layer
                .metrics()
                .counter(metrics::REQUESTS_TOTAL, &[("host", "_OTHER")])
                .is_none()
        );
        assert_eq!(layer.dropped_lines(), 0, "the line is still offered");
    }

    #[test]
    fn the_shared_layer_is_one_instance_for_the_process() {
        let first = Observability::shared();
        let second = Observability::shared();
        assert!(Arc::ptr_eq(&first, &second));
        // The registry flag the shared layer reports is process-global state
        // the gear's own lifecycle drives, and a test that runs the stop path
        // tears it down for the whole process, so it is read where the
        // registry is owned rather than here.
    }

    #[test]
    fn the_observable_gauges_read_the_state_the_source_reports() {
        let layer = layer();
        let source = Arc::new(StubState {
            states: vec![("10.0.0.1", BreakerState::Open)],
            transitions: Mutex::new(vec![transition(
                "10.0.0.1",
                BreakerState::Closed,
                BreakerState::Open,
            )]),
        });
        layer.attach_state_source(source);
        // The transition counter is driven by the breaker's own recorded
        // transitions, read once per closed request.
        let context = context(Some("api.vendor.com"));
        layer.metrics().record_request(&RequestObservation {
            context: &context,
            status: 503,
            upstream_called: false,
            duration: Duration::from_millis(1),
            response_bytes: None,
            tenant_id: None,
            principal_id: None,
            error: None,
        });
        assert_eq!(layer.metrics().breaker_hosts(), vec!["10.0.0.1".to_owned()]);
        let transitions = layer.metrics().counter(
            metrics::BREAKER_TRANSITIONS,
            &[
                ("host", "10.0.0.1"),
                ("from_state", "closed"),
                ("to_state", "open"),
            ],
        );
        assert_eq!(transitions, Some(1), "one recorded transition, one increment");
        assert_eq!(
            layer
                .metrics()
                .breaker_state_of("10.0.0.1")
                .map(BreakerState::as_code),
            Some(BreakerState::Open.as_code())
        );
    }

    #[test]
    fn the_health_gauge_reports_the_alias_and_the_endpoint_pair() {
        let layer = layer();
        let context = context(Some("api.vendor.com"));
        layer.metrics().record_request(&RequestObservation {
            context: &context,
            status: 200,
            upstream_called: true,
            duration: Duration::from_millis(1),
            response_bytes: None,
            tenant_id: None,
            principal_id: None,
            error: None,
        });
        assert_eq!(
            layer.metrics().endpoint_pairs(),
            vec![("api.vendor.com".to_owned(), "10.0.0.1".to_owned())]
        );
    }

    #[test]
    fn the_upstream_phase_is_recorded_for_the_upstream_call() {
        let layer = layer();
        layer.observe_upstream_duration("api.vendor.com", "/v1/things", Duration::from_millis(2));
        assert!(
            layer
                .metrics()
                .histogram(
                    metrics::REQUEST_DURATION,
                    &[
                        ("host", "api.vendor.com"),
                        ("http.route", "/v1/things"),
                        ("phase", "upstream"),
                    ],
                )
                .is_some(),
            "the upstream phase is a separate series"
        );
    }

    #[tokio::test]
    async fn the_drain_is_spawned_once_and_stops_on_the_token() {
        let layer = layer();
        let _ = layer.writer().enable_capture();
        let token = CancellationToken::new();
        let handle = layer.spawn_drain(token.clone());
        assert!(handle.is_some(), "the drain is spawned by the init");
        assert!(
            layer.spawn_drain(token.clone()).is_none(),
            "the receiving half is taken once"
        );
        token.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle.unwrap()).await;
    }

    #[tokio::test]
    async fn a_stop_and_a_fresh_registration_drain_again() {
        // The stop of a first start takes the receiving half of the channel
        // with it: until the next start registers, the lines it offers are
        // refused and counted, and none of them accumulates in the channel.
        let layer = layer();
        let token = CancellationToken::new();
        let first = layer
            .spawn_drain(token.clone())
            .expect("the first start drains");
        assert!(layer.spawn_drain(token.clone()).is_none());
        token.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), first).await;

        let before = layer.writer().drops();
        assert!(
            !layer
                .writer()
                .offer("{\"line\":\"offered after the stop\"}".to_owned()),
            "no drain holds the channel any more"
        );
        assert_eq!(layer.writer().drops(), before + 1);

        // The start that follows the stop registers again, which restores the
        // receiving half, so the drain it spawns writes what is offered.
        layer.teardown();
        layer.register();
        let token = CancellationToken::new();
        let second = layer
            .spawn_drain(token.clone())
            .expect("the start that follows the stop drains again");
        assert!(
            layer
                .writer()
                .offer("{\"line\":\"offered after the start\"}".to_owned()),
            "the restored channel takes the line"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
        token.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), second).await;
        assert_eq!(layer.writer().written(), 1, "the offered line was drained");
    }

    #[test]
    fn an_offer_before_the_drain_runs_is_never_blocking_and_never_counted_as_a_failure() {
        let layer = Arc::new(Observability::new());
        // No drain task was spawned, so the channel is never drained: an offer
        // is still non-blocking and is not a serialization failure.
        let record = AuditRecord::build(&AuditInput {
            context: &context(None),
            status: 200,
            duration: Duration::from_millis(1),
            response_bytes: None,
            error: None,
            tenant_id: None,
            principal_id: None,
            stream: None,
        });
        layer.emit(&record);
        assert_eq!(layer.dropped_lines(), 0, "the offer itself does not drop");
    }

    #[test]
    fn a_successful_line_is_sampled_by_the_fixed_rate_and_a_failure_is_not() {
        let layer = layer();
        let success = AuditRecord::build(&AuditInput {
            context: &context(Some("api.vendor.com")),
            status: 200,
            duration: Duration::from_millis(1),
            response_bytes: None,
            error: None,
            tenant_id: None,
            principal_id: None,
            stream: None,
        });
        for _ in 0..1_000 {
            layer.emit(&success);
        }
        assert_eq!(layer.sampler().emitted_of_success(), 10, "one line in a hundred");
        let failure = AuditRecord::build(&AuditInput {
            context: &context(Some("api.vendor.com")),
            status: 502,
            duration: Duration::from_millis(1),
            response_bytes: None,
            error: Some(&DomainError::DownstreamError {
                detail: "upstream failed".to_owned(),
            }),
            tenant_id: None,
            principal_id: None,
            stream: None,
        });
        for _ in 0..50 {
            layer.emit(&failure);
        }
        assert_eq!(
            layer.sampler().success_seen(),
            1_000,
            "a failure line is never counted as a success"
        );
    }
}
