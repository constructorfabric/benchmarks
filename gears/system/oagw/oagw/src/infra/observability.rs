//! The observability infrastructure of
//! `cpt-cf-oagw-feature-observability`: the nine instruments registered once on
//! the toolkit OpenTelemetry SDK, the stdout audit sink and the
//! [`Telemetry`] facade the producing features emit through.
//!
//! The module owns no decision: the pipeline, the limiter and the management
//! handlers own the decisions behind every instrument and every record, and this
//! module only holds the handles they update and the sink they write through.
//! Everything lives in process-local memory inside the single executable — no
//! table, no schema object, no repository trait, no persistence path, no second
//! process, no sidecar, no log shipper and no metrics forwarder.

// @cpt-begin:cpt-cf-oagw-dod-metric-instruments:p1:inst-full
// The registration contract of `cpt-cf-oagw-dod-metric-instruments`: the nine
// instruments of DESIGN §4.2 that have a producing decision this release are
// registered exactly once, at gear initialization, on the toolkit
// OpenTelemetry SDK the host configures, with exactly the names and the label
// keys that vocabulary fixes and with no other instrument beside them.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Gauge, Histogram, Meter};
use parking_lot::Mutex;

use crate::domain::observability as ob;
use crate::domain::observability::{
    AuditEventClass, AuditFacts, AvailabilityLabels, ErrorLabels, InFlightLabels, Phase,
    RateLimitLabels, RequestOutcomeLabels, SelectionLabels, StageLabels, TargetHostLabels,
};

/// The nine instrument handles the gear registered at initialization.
///
/// Every handle is created exactly once, from the `Meter` the gear resolved at
/// init, and is read-only from then on: no instrument is created, looked up or
/// registered in any emission path, and the three instruments DESIGN names with
/// no producing decision this release are not among them.
pub struct MetricInstruments {
    /// `oagw_requests_total` — one increment per completed proxied request.
    requests_total: Counter<u64>,
    /// `oagw_request_duration_seconds` — one observation per completed stage.
    request_duration: Histogram<f64>,
    /// `oagw_requests_in_flight` — the requests whose outbound call was issued
    /// and is not settled yet.
    requests_in_flight: Gauge<u64>,
    /// `oagw_errors_total` — one increment per failure rendered through a row of
    /// the closed table.
    errors_total: Counter<u64>,
    /// `oagw_rate_limit_exceeded_total` — one increment per refusal.
    rate_limit_exceeded_total: Counter<u64>,
    /// `oagw_rate_limit_usage_ratio` — the token level a check left behind.
    rate_limit_usage_ratio: Gauge<f64>,
    /// `oagw_routing_target_host_used` — a selection that consumed an
    /// `X-OAGW-Target-Host` value.
    routing_target_host_used: Counter<u64>,
    /// `oagw_routing_endpoint_selected` — one increment per selection.
    routing_endpoint_selected: Counter<u64>,
    /// `oagw_upstream_available` — the last transport outcome against an
    /// endpoint.
    upstream_available: Gauge<f64>,
}

/// The names of the nine registered instruments, the only instrument names the
/// process registry can hold.
// @cpt-begin:cpt-cf-oagw-flow-metric-emission:p1:inst-ob-13
// The three instruments DESIGN §4.2 names that have no producing decision this
// release — `oagw_circuit_breaker_state{host}`,
// `oagw_circuit_breaker_transitions_total{host, from_state, to_state}` and
// `oagw_upstream_connections{host, state}` — are absent from this list, so the
// `ELSE` branch of the emission flow names them and emits nothing: no instrument
// is registered for them and no event can reach one.
pub const REGISTERED_INSTRUMENTS: [&str; 9] = [
    ob::METRIC_REQUESTS_TOTAL,
    ob::METRIC_REQUEST_DURATION_SECONDS,
    ob::METRIC_REQUESTS_IN_FLIGHT,
    ob::METRIC_ERRORS_TOTAL,
    ob::METRIC_RATE_LIMIT_EXCEEDED_TOTAL,
    ob::METRIC_RATE_LIMIT_USAGE_RATIO,
    ob::METRIC_ROUTING_TARGET_HOST_USED,
    ob::METRIC_ROUTING_ENDPOINT_SELECTED,
    ob::METRIC_UPSTREAM_AVAILABLE,
];
// @cpt-end:cpt-cf-oagw-flow-metric-emission:p1:inst-ob-13

impl MetricInstruments {
    /// Registers the nine instruments on `meter`, the form gear
    /// initialization calls once, before any request.
    #[must_use]
    pub fn register(meter: &Meter) -> Self {
        // inst-ob-02: the instruments were registered once here, at gear
        // initialization, on the toolkit OpenTelemetry SDK the host configures —
        // the nine instruments of §1.5 and no others — and no instrument is
        // created, looked up or registered in any step of the emission flow
        // below.
        Self {
            requests_total: meter
                .u64_counter(ob::METRIC_REQUESTS_TOTAL)
                .with_description("Proxied requests by upstream, method, route and status.")
                .with_unit("1")
                .build(),
            request_duration: meter
                .f64_histogram(ob::METRIC_REQUEST_DURATION_SECONDS)
                .with_description("Stage durations by upstream, route and pipeline stage.")
                .with_unit(ob::DURATION_UNIT)
                .with_boundaries(ob::DURATION_BUCKETS.to_vec())
                .build(),
            requests_in_flight: meter
                .u64_gauge(ob::METRIC_REQUESTS_IN_FLIGHT)
                .with_description("Requests whose outbound call was issued and is not settled.")
                .with_unit("1")
                .build(),
            errors_total: meter
                .u64_counter(ob::METRIC_ERRORS_TOTAL)
                .with_description("Failures rendered through the closed error table.")
                .with_unit("1")
                .build(),
            rate_limit_exceeded_total: meter
                .u64_counter(ob::METRIC_RATE_LIMIT_EXCEEDED_TOTAL)
                .with_description("Rate-limit refusals by upstream and route.")
                .with_unit("1")
                .build(),
            rate_limit_usage_ratio: meter
                .f64_gauge(ob::METRIC_RATE_LIMIT_USAGE_RATIO)
                .with_description("Token level a rate-limit check left behind.")
                .with_unit("1")
                .build(),
            routing_target_host_used: meter
                .u64_counter(ob::METRIC_ROUTING_TARGET_HOST_USED)
                .with_description("Endpoint selections that consumed an X-OAGW-Target-Host.")
                .with_unit("1")
                .build(),
            routing_endpoint_selected: meter
                .u64_counter(ob::METRIC_ROUTING_ENDPOINT_SELECTED)
                .with_description("Endpoint selections by upstream, host and method.")
                .with_unit("1")
                .build(),
            upstream_available: meter
                .f64_gauge(ob::METRIC_UPSTREAM_AVAILABLE)
                .with_description("Last transport outcome against an endpoint.")
                .with_unit("1")
                .build(),
        }
    }
}
// @cpt-end:cpt-cf-oagw-dod-metric-instruments:p1:inst-full

// @cpt-begin:cpt-cf-oagw-dod-audit-record:p1:inst-full
// The sink contract of `cpt-cf-oagw-dod-audit-record`: every audit record is
// rendered as exactly one structured JSON line on stdout — the one audit sink
// DESIGN §4.3 names — with no second sink, no file, no table, no shipper and no
// telemetry backend connection of this feature's own. The write is best-effort:
// a failure to write is swallowed and never fails the request, whose status,
// headers and body the producing feature has already fixed.

/// The sink a rendered record is written through.
///
/// The trait has one method on purpose: a sink receives one line, writes it and
/// answers nothing, because no emission path may wait for an answer, retry a
/// failed write or buffer a line for later.
pub trait AuditSink: Send + Sync {
    /// Writes one audit record to the sink.
    fn write_line(&self, line: &str);
}

/// The flag the stdout sink reports a failed write through, so one failure is
/// named out-of-band once and a broken stdout never turns into a failure loop
/// of its own.
static STDOUT_FAILURE_REPORTED: AtomicBool = AtomicBool::new(false);

/// The stdout sink of DESIGN §4.3, the only sink this feature has.
#[derive(Debug, Default)]
pub struct StdoutAuditSink;

impl AuditSink for StdoutAuditSink {
    fn write_line(&self, line: &str) {
        // @cpt-begin:cpt-cf-oagw-flow-audit-record:p1:inst-ob-28
        // Best-effort and never failing: a broken stdout is the caller's
        // environment, the record is dropped whole, no retry is issued and the
        // request that produced it is unaffected — the status, headers and body
        // the producing feature fixed are already gone. The failure itself is
        // named once, on stderr and out of band, so a closed stdout does not
        // silence the audit trail invisibly; the line carries the error and
        // nothing of the record it belongs to.
        use std::io::Write as _;
        let mut stdout = std::io::stdout().lock();
        let written = writeln!(stdout, "{line}");
        let outcome = written.and_then(|()| stdout.flush());
        if let Err(error) = outcome
            && !STDOUT_FAILURE_REPORTED.swap(true, Ordering::AcqRel)
        {
            // Written out-of-band and best-effort itself: the diagnostic is
            // dropped as silently as the record was if stderr is gone too, and
            // no path here can fail.
            let _ = writeln!(
                std::io::stderr(),
                "oagw.observability: the stdout audit sink failed, the audit record was \
                 suppressed and no further failure is reported: {error}"
            );
        }
        // @cpt-end:cpt-cf-oagw-flow-audit-record:p1:inst-ob-28
    }
}

/// The sink the tests drive, capturing the lines a rendering produced.
#[derive(Debug, Default)]
pub struct CapturingAuditSink {
    /// The lines the sink received, in the order it received them.
    pub lines: Mutex<Vec<String>>,
}

impl CapturingAuditSink {
    /// The lines the sink received.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        self.lines.lock().clone()
    }
}

impl AuditSink for CapturingAuditSink {
    fn write_line(&self, line: &str) {
        self.lines.lock().push(line.to_owned());
    }
}
// @cpt-end:cpt-cf-oagw-dod-audit-record:p1:inst-full

// @cpt-begin:cpt-cf-oagw-dod-audit-event-coverage:p1:inst-full
// The emission contract of `cpt-cf-oagw-dod-audit-event-coverage`: the one
// facade every producing path emits through, holding the nine handles and the
// one sink. Every path below is an in-memory update of an already-registered
// instrument or one line on the sink — no I/O, no buffer, no queue, no batch and
// no second emission path exists, and no path can fail, delay or re-map the
// request that produced the event.

/// The decision event the proxy pipeline already took and recorded
/// (`inst-ob-01`), carrying the outcome, the stages the request completed and
/// the measures the pipeline holds. Nothing is re-derived from the request here
/// and no upstream document's decision is re-taken.
#[derive(Debug, Clone)]
pub struct RequestOutcome {
    /// The upstream alias the resolved configuration carries, the `host` label.
    pub host: String,
    /// The request method, normalized by the label vocabulary.
    pub method: String,
    /// The normalized route match pattern the pipeline matched.
    pub route: String,
    /// The numeric status of the response the caller receives.
    pub status: u16,
    /// True when the status is the upstream's own, false when the gateway
    /// rendered it.
    pub passed_through: bool,
    /// The row of the closed table the pipeline rendered, when it did.
    pub error: Option<ob::AuditErrorRow>,
    /// One entry per completed pipeline stage, in that flow's stage order.
    pub stages: Vec<(Phase, Duration)>,
    /// True when the outbound call was issued, the in-flight gauge being
    /// decremented only for such a request.
    pub issued: bool,
    /// The whole-request duration, measured from the arrival instant.
    pub duration_ms: u64,
    /// The size of the received request body.
    pub request_size: u64,
    /// The size of the body handed back.
    pub response_size: u64,
    /// The security context the request arrived with.
    pub tenant_id: Option<String>,
    /// The authenticated subject of that security context.
    pub principal_id: Option<String>,
    /// The platform trace context the request arrived with.
    pub request_id: Option<String>,
    /// The gear-relative request path.
    pub path: String,
}

/// The one emission facade of the gear: the nine handles, the audit sink and the
/// sampling counters, all in process-local memory.
///
/// The facade is `Arc`-shared and `Clone`, and every method is infallible: no
/// emission can fail, delay, buffer, queue, batch or re-map the request that
/// produced the event, and no emission ever alters a response. The counters are
/// shared by reference and carry their own synchronization, so no emission takes
/// a lock the gate it is admitted through does not need.
#[derive(Clone)]
pub struct Telemetry {
    metrics: Arc<MetricInstruments>,
    sink: Arc<dyn AuditSink>,
    counters: Arc<ob::SamplingCounters>,
    /// The per-host in-flight level, process-local memory a restart resets.
    in_flight: Arc<Mutex<HashMap<String, u64>>>,
}

impl std::fmt::Debug for Telemetry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Telemetry").finish_non_exhaustive()
    }
}

impl Telemetry {
    /// Builds the facade over the handles the gear registered and the sink it
    /// installed, both at initialization and before any request.
    #[must_use]
    pub fn new(metrics: MetricInstruments, sink: Arc<dyn AuditSink>) -> Self {
        Self::shared(Arc::new(metrics), sink)
    }

    /// Builds the facade over handles already `Arc`-shared, the form the test
    /// harness uses when the same handles must be collected and emitted.
    #[must_use]
    pub fn shared(metrics: Arc<MetricInstruments>, sink: Arc<dyn AuditSink>) -> Self {
        Self {
            metrics,
            sink,
            counters: Arc::new(ob::SamplingCounters::new()),
            in_flight: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The instruments the facade updates, the form the gear shares.
    #[must_use]
    pub fn instruments(&self) -> &MetricInstruments {
        &self.metrics
    }

    /// Emits the request outcome of one proxied request (`inst-ob-03` to
    /// `inst-ob-07`).
    ///
    /// Exactly one `oagw_requests_total` increment and one duration observation
    /// per completed stage are produced, the in-flight gauge is decremented only
    /// when the outbound call was issued, and `oagw_errors_total` is incremented
    /// once only when the pipeline rendered the failure through a row of the
    /// closed table — a passed-through upstream error status is not counted by
    /// it and is visible instead in the numeric status label of
    /// `oagw_requests_total`.
    pub fn request_outcome(&self, outcome: &RequestOutcome) {
        // @cpt-begin:cpt-cf-oagw-flow-metric-emission:p1:inst-ob-01
        // The event is the decision the pipeline already took and recorded: the
        // arrival instant, the stages the request completed, the mapped row, the
        // passed-through status and the response size. No attribute is
        // re-derived from the request here and no upstream document's decision
        // is re-taken.
        let labels = ob::deny_identity(
            RequestOutcomeLabels {
                host: outcome.host.clone(),
                method: ob::normalize_method(&outcome.method),
                route: ob::normalize_route(Some(&outcome.route)),
                status: outcome.status,
            }
            .compose(),
        );
        let attrs = attributes(&labels);
        // @cpt-begin:cpt-cf-oagw-flow-metric-emission:p1:inst-ob-03
        // @cpt-begin:cpt-cf-oagw-flow-metric-emission:p1:inst-ob-04
        // One increment per completed proxied request, under the label set the
        // normalization returned, `host` carrying the upstream alias and the
        // numeric status of the response the caller receives.
        self.metrics.requests_total.add(1, &attrs);
        // @cpt-end:cpt-cf-oagw-flow-metric-emission:p1:inst-ob-04
        // @cpt-begin:cpt-cf-oagw-flow-metric-emission:p1:inst-ob-05
        // One observation per completed pipeline stage under that stage's
        // `phase` label, against the twelve buckets and no other boundary.
        for (phase, elapsed) in &outcome.stages {
            let stage = attributes(&ob::deny_identity(
                StageLabels {
                    host: outcome.host.clone(),
                    route: ob::normalize_route(Some(&outcome.route)),
                    phase: *phase,
                }
                .compose(),
            ));
            self.metrics
                .request_duration
                .record(elapsed.as_secs_f64(), &stage);
        }
        // @cpt-end:cpt-cf-oagw-flow-metric-emission:p1:inst-ob-05
        // @cpt-begin:cpt-cf-oagw-flow-metric-emission:p1:inst-ob-06
        // The gauge is decremented for the request being settled, and only for a
        // request whose outbound call was issued, so a request the pipeline
        // refused before the issuance is never decremented and the gauge is
        // never driven negative.
        if outcome.issued {
            self.settle_in_flight(&outcome.host);
        }
        // @cpt-end:cpt-cf-oagw-flow-metric-emission:p1:inst-ob-06
        // @cpt-begin:cpt-cf-oagw-flow-metric-emission:p1:inst-ob-07
        // A failure rendered through a row of the closed table increments the
        // error counter once with that row's name; a passed-through upstream
        // error status is not counted here.
        if !outcome.passed_through
            && let Some(row) = outcome.error.as_ref()
        {
            let errors = attributes(&ob::deny_identity(
                ErrorLabels {
                    host: outcome.host.clone(),
                    route: ob::normalize_route(Some(&outcome.route)),
                    error_type: row.error_type,
                }
                .compose(),
            ));
            self.metrics.errors_total.add(1, &errors);
        }
        // @cpt-end:cpt-cf-oagw-flow-metric-emission:p1:inst-ob-07
        // @cpt-begin:cpt-cf-oagw-flow-metric-emission:p1:inst-ob-14
        // Every path above is an in-memory update of an already-registered
        // instrument — no I/O, no buffer, no queue, no batch and no second
        // emission path exists — and the update is visible to the process
        // registry the platform telemetry stack scrapes.
        // @cpt-end:cpt-cf-oagw-flow-metric-emission:p1:inst-ob-14
        // @cpt-begin:cpt-cf-oagw-flow-metric-emission:p1:inst-ob-16
        // The updated instrument state is returned to that registry, the
        // response the caller receives being the producing feature's and
        // unchanged by anything this flow performed.
        // @cpt-end:cpt-cf-oagw-flow-metric-emission:p1:inst-ob-16
        // @cpt-end:cpt-cf-oagw-flow-metric-emission:p1:inst-ob-03
        // @cpt-end:cpt-cf-oagw-flow-metric-emission:p1:inst-ob-01
    }

    /// Emits the issuance of an outbound call (`inst-ob-08`).
    ///
    /// The in-flight gauge is incremented once for the request whose call is
    /// issued, the value being process-local and never synchronized with any
    /// other process.
    pub fn outbound_issued(&self, host: &str) {
        // @cpt-begin:cpt-cf-oagw-flow-metric-emission:p1:inst-ob-08
        let value = {
            let mut counts = self.in_flight.lock();
            let entry = counts.entry(host.to_owned()).or_insert(0);
            *entry = entry.saturating_add(1);
            *entry
        };
        self.record_in_flight(host, value);
        // @cpt-end:cpt-cf-oagw-flow-metric-emission:p1:inst-ob-08
    }

    /// Settles one issued request, the gauge never being driven negative: the
    /// count a restart resets is held in process-local memory.
    fn settle_in_flight(&self, host: &str) {
        let value = {
            let mut counts = self.in_flight.lock();
            let entry = counts.entry(host.to_owned()).or_insert(0);
            *entry = entry.saturating_sub(1);
            *entry
        };
        self.record_in_flight(host, value);
    }

    /// Records the in-flight level of one host under that host's label.
    fn record_in_flight(&self, host: &str, value: u64) {
        let in_flight = attributes(&ob::deny_identity(
            InFlightLabels {
                host: host.to_owned(),
            }
            .compose(),
        ));
        self.metrics.requests_in_flight.record(value, &in_flight);
    }

    /// Emits an endpoint selection (`inst-ob-09`).
    ///
    /// The selection method carries `explicit_header`, `round_robin` or
    /// `default` as the endpoint-selection algorithm recorded it, and the
    /// target-host counter is incremented when the selection consumed an
    /// `X-OAGW-Target-Host` value that named the endpoint.
    pub fn endpoint_selected(
        &self,
        upstream_id: &str,
        endpoint_host: &str,
        selection_method: &'static str,
        target_host_used: bool,
    ) {
        // @cpt-begin:cpt-cf-oagw-flow-metric-emission:p1:inst-ob-09
        let selected = attributes(&ob::deny_identity(
            SelectionLabels {
                upstream_id: upstream_id.to_owned(),
                endpoint_host: endpoint_host.to_owned(),
                selection_method,
            }
            .compose(),
        ));
        self.metrics.routing_endpoint_selected.add(1, &selected);
        if target_host_used {
            let used = attributes(&ob::deny_identity(
                TargetHostLabels {
                    upstream_id: upstream_id.to_owned(),
                    endpoint_host: endpoint_host.to_owned(),
                }
                .compose(),
            ));
            self.metrics.routing_target_host_used.add(1, &used);
        }
        // @cpt-end:cpt-cf-oagw-flow-metric-emission:p1:inst-ob-09
    }

    /// Emits an upstream availability outcome (`inst-ob-10`).
    ///
    /// The gauge is set to 0 when the pipeline classified the exchange
    /// `LinkUnavailable` or `ConnectionTimeout` against that endpoint and to 1
    /// when an exchange against it completed, no probe being sent and no health
    /// check being configured.
    pub fn availability(&self, host: &str, endpoint: &str, up: bool) {
        // @cpt-begin:cpt-cf-oagw-flow-metric-emission:p1:inst-ob-10
        let available = attributes(&ob::deny_identity(
            AvailabilityLabels {
                host: host.to_owned(),
                endpoint: endpoint.to_owned(),
            }
            .compose(),
        ));
        self.metrics
            .upstream_available
            .record(f64::from(u8::from(up)), &available);
        // @cpt-end:cpt-cf-oagw-flow-metric-emission:p1:inst-ob-10
    }

    /// Emits the refusal the rate-limit check produced (`inst-ob-11`).
    ///
    /// The `path` label carries the normalized route match pattern of the route
    /// the check evaluated.
    pub fn rate_limit_refused(&self, host: &str, path: &str) {
        // @cpt-begin:cpt-cf-oagw-flow-metric-emission:p1:inst-ob-11
        let refused = attributes(&ob::deny_identity(
            RateLimitLabels {
                host: host.to_owned(),
                path: path.to_owned(),
            }
            .compose(),
        ));
        self.metrics.rate_limit_exceeded_total.add(1, &refused);
        // @cpt-end:cpt-cf-oagw-flow-metric-emission:p1:inst-ob-11
    }

    /// Emits the token level a check left behind (`inst-ob-12`).
    ///
    /// The ratio is the level the bucket represents over the effective limit the
    /// hierarchical merge computed, clamped into 0.0 to 1.0.
    pub fn rate_limit_usage(&self, host: &str, path: &str, ratio: f64) {
        // @cpt-begin:cpt-cf-oagw-flow-metric-emission:p1:inst-ob-12
        let usage = attributes(&ob::deny_identity(
            RateLimitLabels {
                host: host.to_owned(),
                path: path.to_owned(),
            }
            .compose(),
        ));
        self.metrics
            .rate_limit_usage_ratio
            .record(ratio.clamp(0.0, 1.0), &usage);
        // @cpt-end:cpt-cf-oagw-flow-metric-emission:p1:inst-ob-12
    }

    /// Emits the streamed classification a streamed body reported
    /// (`inst-ob-15`), the abort and the idle timeout of a stream whose head
    /// was already relayed arriving as an ordinary row of the closed table.
    ///
    /// One `oagw_errors_total` increment is produced under the same label keys
    /// as any other failure, and the record the class renders is written the way
    /// every other record is. No request counter is touched: the request the
    /// stream belonged to was already counted exactly once at its own outcome,
    /// and no streamed instrument or streamed label value exists.
    pub fn stream_failure(
        &self,
        host: &str,
        route: &str,
        class: AuditEventClass,
        facts: AuditFacts,
    ) {
        // @cpt-begin:cpt-cf-oagw-flow-metric-emission:p1:inst-ob-15
        // The failure the streamed classification mapped onto a row of the
        // closed table is counted by `oagw_errors_total` alone, the request
        // counter having been fed once by the outcome of the request the stream
        // belonged to.
        let row = match facts.error.as_ref() {
            Some(row) => row.error_type,
            None => return,
        };
        let errors = attributes(&ob::deny_identity(
            ErrorLabels {
                host: host.to_owned(),
                route: ob::normalize_route(Some(route)),
                error_type: row,
            }
            .compose(),
        ));
        self.metrics.errors_total.add(1, &errors);
        self.audit(class, facts);
        // @cpt-end:cpt-cf-oagw-flow-metric-emission:p1:inst-ob-15
    }

    /// Emits one audit record for the event class a producing path opened
    /// (`inst-ob-17` to `inst-ob-29`).
    ///
    /// The record is filled from the values the producing path already holds,
    /// filtered and rendered by the redaction algorithm, and written as exactly
    /// one structured JSON line on the sink. Nothing is re-read from a store,
    /// nothing is re-derived, the write is best-effort and the producing path
    /// continues — or has already finished — independently of whether the record
    /// was written.
    pub fn audit(&self, class: AuditEventClass, facts: AuditFacts) {
        // @cpt-begin:cpt-cf-oagw-flow-audit-record:p1:inst-ob-17
        // The event arrives from a producing path — the create, replace or
        // delete decision of the management API on an upstream, route or plugin
        // resource, or a request outcome of the proxy pipeline — carrying the
        // outcome, the security context and the values the record is filled
        // from. Nothing is re-read from a store here.
        // @cpt-begin:cpt-cf-oagw-flow-audit-record:p1:inst-ob-24
        // The remaining fields are filled from the values the producing path
        // already holds — timestamp, request_id, host, path, method, status,
        // duration_ms, request_size and response_size — so the 14-field base
        // set of DESIGN §4.3 is complete and no field outside it exists.
        let timestamp = ob::rfc3339(SystemTime::now());
        let error = facts.error.filter(|_| class.carries_error());
        let event = ob::AuditEvent {
            timestamp,
            level: class.level(),
            event: class.event_name(),
            request_id: facts.request_id,
            tenant_id: facts.tenant_id,
            principal_id: facts.principal_id,
            host: facts.host,
            path: facts.path,
            method: facts.method,
            status: facts.status,
            duration_ms: facts.duration_ms,
            request_size: facts.request_size,
            response_size: facts.response_size,
            error_type: error.as_ref().map(|row| row.error_type.to_owned()),
            error_message: error.map(|row| row.error_message),
        };
        // @cpt-end:cpt-cf-oagw-flow-audit-record:p1:inst-ob-24
        // @cpt-begin:cpt-cf-oagw-flow-audit-record:p1:inst-ob-25
        // The filled record is handed to the redaction and rendering filter of
        // `cpt-cf-oagw-algo-audit-redaction`, which takes back either a record
        // ready to write or the suppressed outcome. The counters are shared, so
        // no lock is taken around the gate: a successful request reaches the
        // sampling counter alone, and a record the gate cannot drop reaches none.
        let outcome = ob::render(event, &class, &self.counters, Instant::now());
        // @cpt-end:cpt-cf-oagw-flow-audit-record:p1:inst-ob-25
        // @cpt-begin:cpt-cf-oagw-flow-audit-record:p1:inst-ob-29
        match outcome {
            ob::AuditOutcome::Rendered(event) => {
                // inst-ob-52: the 14 base fields are filled and the filter
                // returned the record ready to write.
                debug_assert!(
                    ob::AuditRecordState::Collected
                        .transition(ob::AuditRecordState::Rendered)
                        .is_some(),
                    "Collected -> Rendered is a declared transition"
                );
                // @cpt-begin:cpt-cf-oagw-flow-audit-record:p1:inst-ob-27
                // Exactly one structured JSON line is serialized onto stdout,
                // the audit sink of DESIGN §4.3, with no second sink, no file,
                // no table, no shipper and no telemetry backend connection of
                // this feature's own.
                self.sink.write_line(&event.to_json_line());
                // @cpt-end:cpt-cf-oagw-flow-audit-record:p1:inst-ob-27
                // inst-ob-54: the line reached stdout, which is the
                // `Rendered` to `Written` transition.
                debug_assert!(
                    ob::AuditRecordState::Rendered
                        .transition(ob::AuditRecordState::Written)
                        .is_some(),
                    "Rendered -> Written is a declared transition"
                );
            }
            ob::AuditOutcome::Suppressed => {
                // @cpt-begin:cpt-cf-oagw-flow-audit-record:p1:inst-ob-26
                // The filter returned the suppressed outcome, so the function
                // returns without writing: no compensating counter is
                // incremented, nothing is buffered for a later write, and the
                // request that produced the event is unaffected.
                // inst-ob-55: the record was dropped after it was collected —
                // by the redaction filter, by the sampling gate or by a failed
                // stdout write — which is the transition into `Suppressed`.
                debug_assert!(
                    ob::AuditRecordState::Collected
                        .transition(ob::AuditRecordState::Suppressed)
                        .is_some(),
                    "Collected -> Suppressed is a declared transition"
                );
                // @cpt-end:cpt-cf-oagw-flow-audit-record:p1:inst-ob-26
            }
        }
        // The written record is returned to the producing path, which continues
        // — or has already finished — independently of whether the record was
        // written.
        // @cpt-end:cpt-cf-oagw-flow-audit-record:p1:inst-ob-29
        // @cpt-end:cpt-cf-oagw-flow-audit-record:p1:inst-ob-17
    }
}

/// Converts a composed label set into the attribute slice the SDK records.
fn attributes(labels: &[ob::Label]) -> Vec<KeyValue> {
    labels
        .iter()
        .map(|label| KeyValue::new(label.key, label.value.clone()))
        .collect()
}
// @cpt-end:cpt-cf-oagw-dod-audit-event-coverage:p1:inst-full

#[cfg(test)]
pub(crate) mod harness {
    //! The capturing metrics exporter the tests read a collected snapshot
    //! through: the SDK's own reader is unusable in a test and the
    //! `ResourceMetrics` fields are `pub(crate)`, so an exporter that snapshots
    //! the collection instead of transmitting it is the only path that works.

    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use opentelemetry::metrics::MeterProvider as _;
    use opentelemetry_sdk::error::OTelSdkError as ExportError;
    use opentelemetry_sdk::metrics::data::{
        AggregatedMetrics, Metric, MetricData, ResourceMetrics,
    };
    use opentelemetry_sdk::metrics::exporter::PushMetricExporter;
    use opentelemetry_sdk::metrics::{SdkMeterProvider, Temporality};

    use crate::infra::observability::MetricInstruments;

    /// One collected series: the instrument name, its label set and the value it
    /// holds, or the bucket counts when the instrument is the duration
    /// histogram.
    #[derive(Debug, Clone, PartialEq)]
    pub struct Series {
        /// The instrument name.
        pub name: String,
        /// The label set, as key-value pairs.
        pub attributes: Vec<(String, String)>,
        /// The value of the data point, for a gauge and a sum.
        pub value: Option<f64>,
        /// The count of each bucket, for the duration histogram.
        pub bucket_counts: Option<Vec<u64>>,
        /// The upper bounds of the buckets, for the duration histogram.
        pub bounds: Option<Vec<f64>>,
        /// The unit the instrument was registered with.
        pub unit: String,
    }

    impl Series {
        /// The value of one label of the series.
        #[must_use]
        pub fn value_of(&self, key: &str) -> Option<&str> {
            self.attributes
                .iter()
                .find(|(label_key, _)| label_key == key)
                .map(|(_, value)| value.as_str())
        }
    }

    /// The exporter that snapshots every collection instead of transmitting it.
    #[derive(Debug, Clone, Default)]
    pub struct CapturingExporter {
        collected: Arc<Mutex<Vec<Series>>>,
    }

    impl CapturingExporter {
        /// An exporter whose clones share one buffer.
        #[must_use]
        pub fn new() -> Self {
            Self::default()
        }

        /// The series collected so far, the buffer drained.
        #[must_use]
        pub fn take(&self) -> Vec<Series> {
            std::mem::take(&mut self.collected.lock().unwrap())
        }
    }

    impl PushMetricExporter for CapturingExporter {
        async fn export(&self, metrics: &ResourceMetrics) -> Result<(), ExportError> {
            let mut collected = self.collected.lock().unwrap();
            for scope in metrics.scope_metrics() {
                for metric in scope.metrics() {
                    collect_metric(metric, &mut collected);
                }
            }
            Ok(())
        }

        fn force_flush(&self) -> Result<(), ExportError> {
            Ok(())
        }

        fn shutdown_with_timeout(&self, _timeout: Duration) -> Result<(), ExportError> {
            Ok(())
        }

        fn temporality(&self) -> Temporality {
            Temporality::Cumulative
        }
    }

    fn collect_metric(metric: &Metric, collected: &mut Vec<Series>) {
        let name = metric.name().to_owned();
        let unit = metric.unit().to_owned();
        let mut push = |attributes: Vec<(String, String)>,
                        value: Option<f64>,
                        bucket_counts: Option<Vec<u64>>,
                        bounds: Option<Vec<f64>>| {
            collected.push(Series {
                name: name.clone(),
                attributes,
                value,
                bucket_counts,
                bounds,
                unit: unit.clone(),
            });
        };
        match metric.data() {
            AggregatedMetrics::F64(MetricData::Gauge(data)) => {
                for point in data.data_points() {
                    push(pairs(point.attributes()), Some(point.value()), None, None);
                }
            }
            AggregatedMetrics::F64(MetricData::Sum(data)) => {
                for point in data.data_points() {
                    push(pairs(point.attributes()), Some(point.value()), None, None);
                }
            }
            AggregatedMetrics::U64(MetricData::Gauge(data)) => {
                for point in data.data_points() {
                    push(
                        pairs(point.attributes()),
                        Some(point.value() as f64),
                        None,
                        None,
                    );
                }
            }
            AggregatedMetrics::U64(MetricData::Sum(data)) => {
                for point in data.data_points() {
                    push(
                        pairs(point.attributes()),
                        Some(point.value() as f64),
                        None,
                        None,
                    );
                }
            }
            AggregatedMetrics::I64(MetricData::Gauge(data)) => {
                for point in data.data_points() {
                    push(
                        pairs(point.attributes()),
                        Some(point.value() as f64),
                        None,
                        None,
                    );
                }
            }
            AggregatedMetrics::I64(MetricData::Sum(data)) => {
                for point in data.data_points() {
                    push(
                        pairs(point.attributes()),
                        Some(point.value() as f64),
                        None,
                        None,
                    );
                }
            }
            AggregatedMetrics::F64(MetricData::Histogram(data)) => {
                for point in data.data_points() {
                    push(
                        pairs(point.attributes()),
                        None,
                        Some(point.bucket_counts().collect()),
                        Some(point.bounds().collect()),
                    );
                }
            }
            AggregatedMetrics::U64(MetricData::Histogram(data)) => {
                for point in data.data_points() {
                    push(
                        pairs(point.attributes()),
                        None,
                        Some(point.bucket_counts().collect()),
                        Some(point.bounds().collect()),
                    );
                }
            }
            AggregatedMetrics::I64(MetricData::Histogram(data)) => {
                for point in data.data_points() {
                    push(
                        pairs(point.attributes()),
                        None,
                        Some(point.bucket_counts().collect()),
                        Some(point.bounds().collect()),
                    );
                }
            }
            AggregatedMetrics::F64(MetricData::ExponentialHistogram(_))
            | AggregatedMetrics::U64(MetricData::ExponentialHistogram(_))
            | AggregatedMetrics::I64(MetricData::ExponentialHistogram(_)) => {}
        }
    }

    /// Flattens one point's attribute set into key-value pairs.
    fn pairs<'a, I>(attributes: I) -> Vec<(String, String)>
    where
        I: IntoIterator<Item = &'a opentelemetry::KeyValue>,
    {
        attributes
            .into_iter()
            .map(|attribute| {
                (
                    attribute.key.as_str().to_owned(),
                    attribute.value.to_string(),
                )
            })
            .collect()
    }

    /// The provider and the instruments one metrics test drives, the provider
    /// kept alive for the whole test and the global provider never touched.
    pub struct MetricsRig {
        /// The provider, dropped last so the reader outlives the instruments.
        provider: SdkMeterProvider,
        /// The exporter the collected series are read from.
        exporter: CapturingExporter,
        /// The instruments the tests record through, shared so the same handles
        /// are collected and emitted.
        instruments: Arc<MetricInstruments>,
    }

    impl MetricsRig {
        /// Builds a provider over a fresh capturing exporter and registers the
        /// nine instruments on a meter of it, exactly once.
        #[must_use]
        pub fn build() -> Self {
            let exporter = CapturingExporter::new();
            let provider = SdkMeterProvider::builder()
                .with_periodic_exporter(exporter.clone())
                .build();
            let meter = provider.meter("oagw");
            Self {
                instruments: Arc::new(MetricInstruments::register(&meter)),
                provider,
                exporter,
            }
        }

        /// The emission facade over the rig's own handles and `sink`.
        #[must_use]
        pub fn telemetry(&self, sink: Arc<dyn super::AuditSink>) -> super::Telemetry {
            super::Telemetry::shared(Arc::clone(&self.instruments), sink)
        }

        /// Forces one collection and returns the series it produced.
        pub fn snapshot(&self) -> Vec<Series> {
            let _ = self.provider.force_flush();
            self.exporter.take()
        }
    }

    /// Registers the nine instruments on `meter`, the form the rig uses.
    impl MetricInstruments {
        /// Re-registers the instruments on `meter`, for a test that drives them
        /// without a provider of its own.
        #[must_use]
        pub fn on(meter: &opentelemetry::metrics::Meter) -> Self {
            Self::register(meter)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::harness::MetricsRig;
    use super::*;
    use crate::domain::observability::{
        AUTH_AUDIT_PER_SEC, AuditErrorRow, DURATION_BUCKETS, DURATION_UNIT, LABEL_ENDPOINT,
        LABEL_ENDPOINT_HOST, LABEL_ERROR_TYPE, LABEL_HOST, LABEL_HTTP_REQUEST_METHOD,
        LABEL_HTTP_RESPONSE_STATUS_CODE, LABEL_HTTP_ROUTE, LABEL_PATH, LABEL_PHASE,
        LABEL_SELECTION_METHOD, LABEL_UPSTREAM_ID, METRIC_ERRORS_TOTAL,
        METRIC_RATE_LIMIT_EXCEEDED_TOTAL, METRIC_RATE_LIMIT_USAGE_RATIO,
        METRIC_REQUEST_DURATION_SECONDS, METRIC_REQUESTS_IN_FLIGHT, METRIC_REQUESTS_TOTAL,
        METRIC_ROUTING_ENDPOINT_SELECTED, METRIC_ROUTING_TARGET_HOST_USED,
        METRIC_UPSTREAM_AVAILABLE, PROXY_SHELL_ROUTE, SUCCESS_SAMPLE, UNIMPLEMENTED_INSTRUMENTS,
    };

    const ALIAS: &str = "payments";
    const ROUTE: &str = "/v1/payments/{id}";
    const ENDPOINT_HOST: &str = "upstream.internal";
    const UPSTREAM_ID: &str = "0a1b2c3d-4e5f-4a6b-8c9d-0e1f2a3b4c5d";

    fn sink() -> Arc<CapturingAuditSink> {
        Arc::new(CapturingAuditSink::default())
    }

    fn outcome(stages: Vec<(Phase, Duration)>, issued: bool) -> RequestOutcome {
        RequestOutcome {
            host: ALIAS.to_owned(),
            method: "GET".to_owned(),
            route: ROUTE.to_owned(),
            status: 200,
            passed_through: true,
            error: None,
            stages,
            issued,
            duration_ms: 4,
            request_size: 1,
            response_size: 2,
            tenant_id: Some("0b6c1a4e-2b0c-4d9f-9a1e-6f0f9b1f2a10".to_owned()),
            principal_id: Some("0c7d2b5f-3c1d-4e0a-0b2f-7a1a0c2a3b11".to_owned()),
            request_id: Some("4bf92f3577b34da6a3ce929d0e0e4736".to_owned()),
            path: "/oagw/v1/proxy/payments/v1/payments/42".to_owned(),
        }
    }

    fn one_stage() -> Vec<(Phase, Duration)> {
        vec![(Phase::OutboundCall, Duration::from_millis(1))]
    }

    // ------------------------------------------------------------------
    // The nine instruments
    // ------------------------------------------------------------------

    #[test]
    fn a_request_outcome_counts_one_request_under_the_design_labels() {
        let rig = MetricsRig::build();
        let sink = sink();
        let telemetry = rig.telemetry(sink);
        telemetry.request_outcome(&outcome(one_stage(), true));
        let snapshot = rig.snapshot();
        let requests: Vec<&super::harness::Series> = snapshot
            .iter()
            .filter(|series| series.name == METRIC_REQUESTS_TOTAL)
            .collect();
        assert_eq!(requests.len(), 1, "one increment, one series: {snapshot:?}");
        assert_eq!(requests[0].value, Some(1.0));
        assert_eq!(requests[0].value_of(LABEL_HOST), Some(ALIAS));
        assert_eq!(requests[0].value_of(LABEL_HTTP_REQUEST_METHOD), Some("GET"));
        assert_eq!(requests[0].value_of(LABEL_HTTP_ROUTE), Some(ROUTE));
        assert_eq!(
            requests[0].value_of(LABEL_HTTP_RESPONSE_STATUS_CODE),
            Some("200")
        );
        // No label set carries a tenant or a principal, and no instrument
        // records the security context the facade was handed.
        assert!(!snapshot.iter().any(|series| {
            series
                .attributes
                .iter()
                .any(|(key, _)| key.contains("tenant") || key.contains("principal"))
        }));
    }

    #[test]
    fn one_observation_is_recorded_per_completed_stage_over_the_twelve_buckets() {
        let rig = MetricsRig::build();
        let stages = vec![
            (Phase::Classification, Duration::from_millis(1)),
            (Phase::RouteMatching, Duration::from_millis(2)),
            (Phase::OutboundCall, Duration::from_millis(3)),
            (Phase::ResponsePassthrough, Duration::from_millis(4)),
        ];
        let telemetry = rig.telemetry(sink());
        telemetry.request_outcome(&outcome(stages.clone(), true));
        let snapshot = rig.snapshot();
        let durations: Vec<&super::harness::Series> = snapshot
            .iter()
            .filter(|series| series.name == METRIC_REQUEST_DURATION_SECONDS)
            .collect();
        assert_eq!(
            durations.len(),
            4,
            "one observation per completed stage: {durations:?}"
        );
        for (phase, _) in &stages {
            let series = durations
                .iter()
                .find(|series| series.value_of(LABEL_PHASE) == Some(phase.as_str()))
                .unwrap_or_else(|| panic!("no series for {}: {durations:?}", phase.as_str()));
            assert_eq!(series.unit, DURATION_UNIT);
            assert_eq!(series.bounds, Some(DURATION_BUCKETS.to_vec()));
            let counts = series.bucket_counts.clone().unwrap_or_default();
            // One bucket per declared boundary plus the implicit +Inf one.
            assert_eq!(counts.len(), DURATION_BUCKETS.len() + 1);
        }
        // A stage the request did not complete is observed no time.
        let absent = [Phase::EndpointSelection, Phase::HeaderValidation]
            .iter()
            .any(|phase| {
                durations
                    .iter()
                    .any(|series| series.value_of(LABEL_PHASE) == Some(phase.as_str()))
            });
        assert!(!absent, "an uncompleted stage is never observed");
    }

    #[test]
    fn the_in_flight_gauge_is_incremented_at_issuance_and_settled_at_outcome() {
        let rig = MetricsRig::build();
        let telemetry = rig.telemetry(sink());
        telemetry.outbound_issued(ALIAS);
        telemetry.outbound_issued(ALIAS);
        let snapshot = rig.snapshot();
        let raised: Vec<&super::harness::Series> = snapshot
            .iter()
            .filter(|series| series.name == METRIC_REQUESTS_IN_FLIGHT)
            .collect();
        assert_eq!(raised.len(), 1);
        assert_eq!(raised[0].value, Some(2.0));
        assert_eq!(raised[0].value_of(LABEL_HOST), Some(ALIAS));
        // Two settlements bring the gauge back to zero: one data point only.
        let settled = rig.telemetry(sink());
        settled.request_outcome(&outcome(one_stage(), true));
        settled.request_outcome(&outcome(one_stage(), true));
        let snapshot = rig.snapshot();
        let settled_series: Vec<&super::harness::Series> = snapshot
            .iter()
            .filter(|series| series.name == METRIC_REQUESTS_IN_FLIGHT)
            .collect();
        assert_eq!(settled_series.len(), 1);
        assert_eq!(settled_series[0].value, Some(0.0));
    }

    #[test]
    fn a_refusal_before_dispatch_leaves_the_in_flight_gauge_unchanged() {
        let rig = MetricsRig::build();
        let telemetry = rig.telemetry(sink());
        // A request refused before the outbound issuance: no issuance, no
        // settlement, and the gauge is never created and never driven negative.
        telemetry.request_outcome(&outcome(one_stage(), false));
        let snapshot = rig.snapshot();
        assert!(
            snapshot
                .iter()
                .all(|series| series.name != METRIC_REQUESTS_IN_FLIGHT),
            "no in-flight series exists: {snapshot:?}"
        );
    }

    #[test]
    fn a_rendered_failure_increments_the_error_counter_with_its_row_name() {
        let rig = MetricsRig::build();
        let telemetry = rig.telemetry(sink());
        let mut rendered = outcome(one_stage(), false);
        rendered.passed_through = false;
        rendered.status = 503;
        rendered.error = Some(AuditErrorRow::of("LinkUnavailable", "Link unavailable"));
        telemetry.request_outcome(&rendered);
        let snapshot = rig.snapshot();
        let errors: Vec<&super::harness::Series> = snapshot
            .iter()
            .filter(|series| series.name == METRIC_ERRORS_TOTAL)
            .collect();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].value, Some(1.0));
        assert_eq!(
            errors[0].value_of(LABEL_ERROR_TYPE),
            Some("LinkUnavailable")
        );
    }

    #[test]
    fn a_passed_through_error_status_is_never_counted_by_the_error_counter() {
        let rig = MetricsRig::build();
        let telemetry = rig.telemetry(sink());
        let mut passed = outcome(one_stage(), true);
        passed.status = 500;
        passed.passed_through = true;
        passed.error = None;
        telemetry.request_outcome(&passed);
        let snapshot = rig.snapshot();
        assert!(
            snapshot
                .iter()
                .all(|series| series.name != METRIC_ERRORS_TOTAL),
            "no error counter was incremented: {snapshot:?}"
        );
        let requests: Vec<&super::harness::Series> = snapshot
            .iter()
            .filter(|series| series.name == METRIC_REQUESTS_TOTAL)
            .collect();
        assert_eq!(
            requests[0].value_of(LABEL_HTTP_RESPONSE_STATUS_CODE),
            Some("500")
        );
    }

    #[test]
    fn a_selection_is_counted_with_its_selection_method_label() {
        let rig = MetricsRig::build();
        let telemetry = rig.telemetry(sink());
        for method in ["default", "round_robin", "explicit_header"] {
            telemetry.endpoint_selected(UPSTREAM_ID, ENDPOINT_HOST, method, false);
        }
        let snapshot = rig.snapshot();
        let selections: Vec<&super::harness::Series> = snapshot
            .iter()
            .filter(|series| series.name == METRIC_ROUTING_ENDPOINT_SELECTED)
            .collect();
        assert_eq!(selections.len(), 3, "one series per selection method");
        for method in ["default", "round_robin", "explicit_header"] {
            let series = selections
                .iter()
                .find(|series| series.value_of(LABEL_SELECTION_METHOD) == Some(method))
                .unwrap_or_else(|| panic!("no series carries {method}: {selections:?}"));
            assert_eq!(series.value_of(LABEL_UPSTREAM_ID), Some(UPSTREAM_ID));
            assert_eq!(series.value_of(LABEL_ENDPOINT_HOST), Some(ENDPOINT_HOST));
            assert_eq!(series.value, Some(1.0));
        }
    }

    #[test]
    fn only_a_target_host_selection_increments_the_target_host_counter() {
        let rig = MetricsRig::build();
        let telemetry = rig.telemetry(sink());
        telemetry.endpoint_selected(UPSTREAM_ID, ENDPOINT_HOST, "default", false);
        telemetry.endpoint_selected(UPSTREAM_ID, ENDPOINT_HOST, "round_robin", false);
        telemetry.endpoint_selected(UPSTREAM_ID, ENDPOINT_HOST, "explicit_header", true);
        let snapshot = rig.snapshot();
        let target_host: Vec<&super::harness::Series> = snapshot
            .iter()
            .filter(|series| series.name == METRIC_ROUTING_TARGET_HOST_USED)
            .collect();
        assert_eq!(
            target_host.len(),
            1,
            "the counter counts only the selection that consumed a target host: {target_host:?}"
        );
        assert_eq!(target_host[0].value, Some(1.0));
        assert_eq!(
            target_host[0].value_of(LABEL_ENDPOINT_HOST),
            Some(ENDPOINT_HOST)
        );
    }

    #[test]
    fn the_availability_gauge_reports_the_transport_outcome() {
        let rig = MetricsRig::build();
        let telemetry = rig.telemetry(sink());
        telemetry.availability(ALIAS, ENDPOINT_HOST, false);
        telemetry.availability(ALIAS, ENDPOINT_HOST, true);
        let snapshot = rig.snapshot();
        let available: Vec<&super::harness::Series> = snapshot
            .iter()
            .filter(|series| series.name == METRIC_UPSTREAM_AVAILABLE)
            .collect();
        // One series per label set; the last value recorded wins.
        assert_eq!(
            available.len(),
            1,
            "one label set, one series: {available:?}"
        );
        assert_eq!(available[0].value, Some(1.0));
        assert_eq!(available[0].value_of(LABEL_HOST), Some(ALIAS));
        assert_eq!(available[0].value_of(LABEL_ENDPOINT), Some(ENDPOINT_HOST));
        assert_eq!(available[0].value, Some(1.0));
    }

    #[test]
    fn the_rate_limit_instruments_carry_the_route_pattern_and_a_clamped_ratio() {
        let rig = MetricsRig::build();
        let telemetry = rig.telemetry(sink());
        telemetry.rate_limit_usage(ALIAS, ROUTE, 1.5);
        telemetry.rate_limit_usage(ALIAS, ROUTE, -0.5);
        telemetry.rate_limit_refused(ALIAS, ROUTE);
        let snapshot = rig.snapshot();
        let usage: Vec<&super::harness::Series> = snapshot
            .iter()
            .filter(|series| series.name == METRIC_RATE_LIMIT_USAGE_RATIO)
            .collect();
        assert_eq!(usage.len(), 1, "one label set, one series: {snapshot:?}");
        assert_eq!(
            usage[0].value,
            Some(0.0),
            "the ratio is clamped to 0.0..=1.0"
        );
        assert_eq!(usage[0].value_of(LABEL_PATH), Some(ROUTE));
        assert_eq!(usage[0].value_of(LABEL_HOST), Some(ALIAS));
        let exceeded: Vec<&super::harness::Series> = snapshot
            .iter()
            .filter(|series| series.name == METRIC_RATE_LIMIT_EXCEEDED_TOTAL)
            .collect();
        assert_eq!(exceeded.len(), 1);
        assert_eq!(exceeded[0].value, Some(1.0));
    }

    #[test]
    fn no_unimplemented_instrument_is_ever_registered() {
        let rig = MetricsRig::build();
        let telemetry = rig.telemetry(sink());
        telemetry.request_outcome(&outcome(one_stage(), true));
        telemetry.endpoint_selected(UPSTREAM_ID, ENDPOINT_HOST, "default", false);
        telemetry.availability(ALIAS, ENDPOINT_HOST, true);
        telemetry.rate_limit_usage(ALIAS, ROUTE, 0.5);
        let snapshot = rig.snapshot();
        for name in UNIMPLEMENTED_INSTRUMENTS {
            assert!(
                snapshot.iter().all(|series| series.name != name),
                "no instrument was registered for {name}"
            );
        }
        let names: Vec<&str> = snapshot.iter().map(|series| series.name.as_str()).collect();
        assert_eq!(
            names.len(),
            names
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            "no instrument is registered twice"
        );
    }

    #[test]
    fn the_route_label_falls_back_to_the_shell_route_never_the_raw_path() {
        let rig = MetricsRig::build();
        let telemetry = rig.telemetry(sink());
        let unmatched = RequestOutcome {
            route: PROXY_SHELL_ROUTE.to_owned(),
            ..outcome(one_stage(), true)
        };
        telemetry.request_outcome(&unmatched);
        let snapshot = rig.snapshot();
        let requests: Vec<&super::harness::Series> = snapshot
            .iter()
            .filter(|series| series.name == METRIC_REQUESTS_TOTAL)
            .collect();
        assert_eq!(
            requests[0].value_of(LABEL_HTTP_ROUTE),
            Some(PROXY_SHELL_ROUTE)
        );
    }

    #[test]
    fn the_nine_instruments_are_registered_with_their_design_label_keys() {
        let rig = MetricsRig::build();
        let telemetry = rig.telemetry(sink());
        // One update of every instrument, so the collection holds all nine.
        telemetry.request_outcome(&outcome(one_stage(), true));
        telemetry.outbound_issued(ALIAS);
        telemetry.endpoint_selected(UPSTREAM_ID, ENDPOINT_HOST, "default", true);
        telemetry.availability(ALIAS, ENDPOINT_HOST, true);
        telemetry.rate_limit_refused(ALIAS, ROUTE);
        telemetry.rate_limit_usage(ALIAS, ROUTE, 0.5);
        let mut rendered = outcome(one_stage(), false);
        rendered.passed_through = false;
        rendered.error = Some(AuditErrorRow::of("LinkUnavailable", "Link unavailable"));
        telemetry.request_outcome(&rendered);
        let snapshot = rig.snapshot();

        let names: std::collections::BTreeSet<&str> =
            snapshot.iter().map(|series| series.name.as_str()).collect();
        assert_eq!(
            names,
            REGISTERED_INSTRUMENTS
                .iter()
                .copied()
                .collect::<std::collections::BTreeSet<&str>>(),
            "the registry holds the nine instruments and no other: {names:?}"
        );
        let expected = [
            (
                METRIC_REQUESTS_TOTAL,
                vec![
                    LABEL_HOST,
                    LABEL_HTTP_REQUEST_METHOD,
                    LABEL_HTTP_ROUTE,
                    LABEL_HTTP_RESPONSE_STATUS_CODE,
                ],
            ),
            (
                METRIC_REQUEST_DURATION_SECONDS,
                vec![LABEL_HOST, LABEL_HTTP_ROUTE, LABEL_PHASE],
            ),
            (METRIC_REQUESTS_IN_FLIGHT, vec![LABEL_HOST]),
            (
                METRIC_ERRORS_TOTAL,
                vec![LABEL_HOST, LABEL_HTTP_ROUTE, LABEL_ERROR_TYPE],
            ),
            (
                METRIC_RATE_LIMIT_EXCEEDED_TOTAL,
                vec![LABEL_HOST, LABEL_PATH],
            ),
            (METRIC_RATE_LIMIT_USAGE_RATIO, vec![LABEL_HOST, LABEL_PATH]),
            (
                METRIC_ROUTING_TARGET_HOST_USED,
                vec![LABEL_UPSTREAM_ID, LABEL_ENDPOINT_HOST],
            ),
            (
                METRIC_ROUTING_ENDPOINT_SELECTED,
                vec![
                    LABEL_UPSTREAM_ID,
                    LABEL_ENDPOINT_HOST,
                    LABEL_SELECTION_METHOD,
                ],
            ),
            (METRIC_UPSTREAM_AVAILABLE, vec![LABEL_HOST, LABEL_ENDPOINT]),
        ];
        for (name, keys) in expected {
            let series: Vec<&super::harness::Series> = snapshot
                .iter()
                .filter(|series| series.name == name)
                .collect();
            assert!(
                !series.is_empty(),
                "{name} was never registered: {snapshot:?}"
            );
            for series in &series {
                let mut carried: Vec<&str> = series
                    .attributes
                    .iter()
                    .map(|(key, _)| key.as_str())
                    .collect();
                carried.sort_unstable();
                let mut wanted = keys.clone();
                wanted.sort_unstable();
                assert_eq!(
                    carried, wanted,
                    "{name} carries exactly its label keys: {:?}",
                    series.attributes
                );
            }
        }
    }

    #[test]
    fn two_requests_on_one_route_are_counted_under_one_route_label() {
        let rig = MetricsRig::build();
        let telemetry = rig.telemetry(sink());
        let mut first = outcome(one_stage(), true);
        first.path = "/oagw/v1/proxy/payments/v1/payments/42".to_owned();
        let mut second = outcome(one_stage(), true);
        second.path = "/oagw/v1/proxy/payments/v1/payments/43".to_owned();
        telemetry.request_outcome(&first);
        telemetry.request_outcome(&second);
        let snapshot = rig.snapshot();
        let requests: Vec<&super::harness::Series> = snapshot
            .iter()
            .filter(|series| series.name == METRIC_REQUESTS_TOTAL)
            .collect();
        assert_eq!(requests.len(), 1, "one route, one series: {snapshot:?}");
        assert_eq!(requests[0].value, Some(2.0));
        assert_eq!(requests[0].value_of(LABEL_HTTP_ROUTE), Some(ROUTE));
        assert!(
            !requests[0]
                .attributes
                .iter()
                .any(|(_, value)| value.ends_with("/42") || value.ends_with("/43")),
            "no raw request path reaches a label: {:?}",
            requests[0].attributes
        );
    }

    #[test]
    fn the_refusal_counter_increments_once_per_refused_request() {
        let rig = MetricsRig::build();
        let telemetry = rig.telemetry(sink());
        for _ in 0..3 {
            telemetry.rate_limit_refused(ALIAS, ROUTE);
        }
        let snapshot = rig.snapshot();
        let exceeded: Vec<&super::harness::Series> = snapshot
            .iter()
            .filter(|series| series.name == METRIC_RATE_LIMIT_EXCEEDED_TOTAL)
            .collect();
        assert_eq!(exceeded.len(), 1, "one label set, one series: {snapshot:?}");
        assert_eq!(exceeded[0].value, Some(3.0));
        // The ratio the effective limit `cpt-cf-oagw-algo-effective-merge`
        // computed stays within 0.0 to 1.0 over the whole range the check
        // reports, including a remaining value below zero.
        for (limit, remaining) in [(100_i64, 40_i64), (10, 0), (5, -5)] {
            let ratio = ob::usage_ratio(limit, remaining);
            assert!(
                (0.0..=1.0).contains(&ratio),
                "the ratio leaves the unit interval: {ratio}"
            );
            telemetry.rate_limit_usage(ALIAS, ROUTE, ratio);
        }
        let snapshot = rig.snapshot();
        let usage: Vec<&super::harness::Series> = snapshot
            .iter()
            .filter(|series| series.name == METRIC_RATE_LIMIT_USAGE_RATIO)
            .collect();
        assert_eq!(usage.len(), 1, "one label set, one series: {snapshot:?}");
        assert!((0.0..=1.0).contains(&usage[0].value.unwrap_or_default()));
    }

    #[test]
    fn the_streamed_classification_counts_the_error_counter_and_nothing_else() {
        let rig = MetricsRig::build();
        let sink = sink();
        let telemetry = rig.telemetry(sink.clone());
        telemetry.stream_failure(
            ALIAS,
            ROUTE,
            AuditEventClass::RequestFailure {
                passed_through_status: None,
            },
            AuditFacts {
                error: Some(AuditErrorRow::of("StreamAborted", "Stream aborted")),
                ..facts(502, None)
            },
        );
        let snapshot = rig.snapshot();
        let errors: Vec<&super::harness::Series> = snapshot
            .iter()
            .filter(|series| series.name == METRIC_ERRORS_TOTAL)
            .collect();
        assert_eq!(errors.len(), 1, "one increment: {snapshot:?}");
        assert_eq!(errors[0].value, Some(1.0));
        assert_eq!(errors[0].value_of(LABEL_ERROR_TYPE), Some("StreamAborted"));
        assert_eq!(errors[0].value_of(LABEL_HOST), Some(ALIAS));
        assert_eq!(errors[0].value_of(LABEL_HTTP_ROUTE), Some(ROUTE));
        // No request counter, no stage observation and no in-flight gauge: the
        // request the stream belonged to was counted once at its own outcome.
        for untouched in [
            METRIC_REQUESTS_TOTAL,
            METRIC_REQUEST_DURATION_SECONDS,
            METRIC_REQUESTS_IN_FLIGHT,
        ] {
            assert!(
                snapshot.iter().all(|series| series.name != untouched),
                "{untouched} is touched by the streamed classification"
            );
        }
        // And the record the class renders is written like every other one.
        let lines = sink.lines();
        assert_eq!(lines.len(), 1, "one record for the streamed failure");
        assert!(lines[0].contains("\"level\":\"ERROR\""), "{}", lines[0]);
        assert!(
            lines[0].contains("\"error_type\":\"StreamAborted\""),
            "{}",
            lines[0]
        );
    }

    #[test]
    fn a_streamed_classification_with_no_row_to_record_emits_nothing() {
        let rig = MetricsRig::build();
        let sink = sink();
        let telemetry = rig.telemetry(sink.clone());
        telemetry.stream_failure(
            ALIAS,
            ROUTE,
            AuditEventClass::RequestFailure {
                passed_through_status: None,
            },
            facts(502, None),
        );
        assert!(
            sink.lines().is_empty(),
            "no record without a row: {:?}",
            sink.lines()
        );
        assert!(
            rig.snapshot()
                .iter()
                .all(|series| series.name != METRIC_ERRORS_TOTAL),
            "no increment without a row"
        );
    }

    #[test]
    fn the_instruments_count_every_request_the_sampling_gate_drops() {
        let rig = MetricsRig::build();
        let sink = sink();
        let telemetry = rig.telemetry(sink.clone());
        let requests = usize::try_from(SUCCESS_SAMPLE).unwrap_or(0) * 3;
        for _ in 0..requests {
            telemetry.request_outcome(&outcome(one_stage(), true));
            telemetry.audit(AuditEventClass::RequestSuccess, facts(200, None));
        }
        let snapshot = rig.snapshot();
        let counted: Vec<&super::harness::Series> = snapshot
            .iter()
            .filter(|series| series.name == METRIC_REQUESTS_TOTAL)
            .collect();
        assert_eq!(counted.len(), 1, "one label set, one series: {snapshot:?}");
        assert_eq!(
            counted[0].value,
            Some(f64::from(SUCCESS_SAMPLE) * 3.0),
            "every request is counted, sampled or not"
        );
        assert_eq!(
            sink.lines().len(),
            3,
            "one record in {SUCCESS_SAMPLE} successes is written"
        );
    }

    #[test]
    fn a_restarted_facade_holds_no_in_flight_state() {
        let rig = MetricsRig::build();
        let telemetry = rig.telemetry(sink());
        telemetry.outbound_issued(ALIAS);
        telemetry.outbound_issued(ALIAS);
        // A restart is a fresh facade over the same instruments: the per-host
        // level is process-local memory the restart does not inherit.
        let restarted = rig.telemetry(sink());
        restarted.request_outcome(&outcome(one_stage(), true));
        let snapshot = rig.snapshot();
        let in_flight: Vec<&super::harness::Series> = snapshot
            .iter()
            .filter(|series| series.name == METRIC_REQUESTS_IN_FLIGHT)
            .collect();
        assert_eq!(in_flight.len(), 1);
        assert_eq!(
            in_flight[0].value,
            Some(0.0),
            "the restart reset the level to zero: {snapshot:?}"
        );
        // The gauge counts from zero again, and is never driven negative.
        restarted.outbound_issued(ALIAS);
        let snapshot = rig.snapshot();
        let in_flight: Vec<&super::harness::Series> = snapshot
            .iter()
            .filter(|series| series.name == METRIC_REQUESTS_IN_FLIGHT)
            .collect();
        assert_eq!(in_flight.len(), 1);
        assert_eq!(in_flight[0].value, Some(1.0));
    }

    #[test]
    fn an_identifier_form_host_is_dropped_and_the_request_still_counted() {
        let rig = MetricsRig::build();
        let telemetry = rig.telemetry(sink());
        let identified = RequestOutcome {
            host: "0b6c1a4e-2b0c-4d9f-9a1e-6f0f9b1f2a10".to_owned(),
            ..outcome(one_stage(), true)
        };
        telemetry.request_outcome(&identified);
        let snapshot = rig.snapshot();
        let requests: Vec<&super::harness::Series> = snapshot
            .iter()
            .filter(|series| series.name == METRIC_REQUESTS_TOTAL)
            .collect();
        assert_eq!(
            requests.len(),
            1,
            "the request is still counted: {snapshot:?}"
        );
        assert_eq!(requests[0].value, Some(1.0));
        assert_eq!(
            requests[0].value_of(LABEL_HOST),
            None,
            "no identifier is carried as a label value: {:?}",
            requests[0].attributes
        );
        assert_eq!(requests[0].value_of(LABEL_HTTP_ROUTE), Some(ROUTE));
        assert_eq!(
            requests[0].value_of(LABEL_HTTP_RESPONSE_STATUS_CODE),
            Some("200")
        );
    }

    // ------------------------------------------------------------------
    // The audit sink
    // ------------------------------------------------------------------

    fn facts(status: u16, error: Option<AuditErrorRow>) -> AuditFacts {
        AuditFacts {
            request_id: Some("4bf92f3577b34da6a3ce929d0e0e4736".to_owned()),
            tenant_id: Some("0a1b2c3d-4e5f-4a6b-8c9d-0e1f2a3b4c5d".to_owned()),
            principal_id: Some("0b1c2d3e-4f5a-4b6c-8d9e-0f1a2b3c4d5e".to_owned()),
            host: ALIAS.to_owned(),
            path: "/oagw/v1/proxy/payments/v1/pay".to_owned(),
            method: "GET".to_owned(),
            status,
            duration_ms: 5,
            request_size: 1,
            response_size: 2,
            error,
        }
    }

    #[test]
    fn a_successful_request_is_written_at_info_with_the_error_type_unset() {
        let rig = MetricsRig::build();
        let sink = sink();
        let telemetry = rig.telemetry(sink.clone());
        for _ in 0..SUCCESS_SAMPLE {
            telemetry.audit(AuditEventClass::RequestSuccess, facts(200, None));
        }
        let lines = sink.lines();
        assert_eq!(lines.len(), 1, "one record in {SUCCESS_SAMPLE} successes");
        assert!(lines[0].contains("\"level\":\"INFO\""));
        assert!(lines[0].contains("\"error_type\":null"));
        assert!(!lines[0].contains("error_message"));
    }

    #[test]
    fn a_failed_request_is_written_at_error_with_its_row() {
        let rig = MetricsRig::build();
        let sink = sink();
        let telemetry = rig.telemetry(sink.clone());
        telemetry.audit(
            AuditEventClass::RequestFailure {
                passed_through_status: None,
            },
            facts(
                503,
                Some(AuditErrorRow::of("LinkUnavailable", "Link unavailable")),
            ),
        );
        let lines = sink.lines();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("\"level\":\"ERROR\""));
        assert!(lines[0].contains("\"error_type\":\"LinkUnavailable\""));
        assert!(
            lines[0].contains("\"error_message\":\"Link unavailable: Upstream link unavailable\"")
        );
    }

    #[test]
    fn a_rate_limit_refusal_is_written_at_warn_and_an_auth_failure_at_error() {
        let rig = MetricsRig::build();
        let sink = sink();
        let telemetry = rig.telemetry(sink.clone());
        telemetry.audit(
            AuditEventClass::RateLimitRefusal {
                retry_after_secs: 7,
            },
            facts(429, None),
        );
        telemetry.audit(AuditEventClass::AuthenticationFailure, facts(401, None));
        let lines = sink.lines();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"level\":\"WARN\""));
        assert!(lines[0].contains("\"event\":\"rate_limit_refusal\""));
        // The refusal names the row of the closed table it is rendered through
        // and the delay the check computed (`inst-ob-21`), the field set of the
        // record being closed so no field of its own exists for the delay.
        assert!(lines[0].contains("\"error_type\":\"RateLimitExceeded\""));
        assert!(lines[0].contains("retry_after_secs=7"));
        assert!(lines[1].contains("\"level\":\"ERROR\""));
        assert!(lines[1].contains("\"event\":\"auth_failure\""));
    }

    #[test]
    fn a_management_change_is_written_at_info_and_never_sampled() {
        let rig = MetricsRig::build();
        let sink = sink();
        let telemetry = rig.telemetry(sink.clone());
        for _ in 0..SUCCESS_SAMPLE * 2 {
            telemetry.audit(
                AuditEventClass::ManagementChange {
                    operation: "create",
                    resource: "upstream",
                },
                facts(201, None),
            );
        }
        assert_eq!(
            sink.lines().len(),
            usize::try_from(SUCCESS_SAMPLE).unwrap_or(0) * 2
        );
    }

    #[test]
    fn the_authentication_class_is_rate_limited_across_the_facade() {
        let rig = MetricsRig::build();
        let sink = sink();
        let telemetry = rig.telemetry(sink.clone());
        for _ in 0..AUTH_AUDIT_PER_SEC * 4 {
            telemetry.audit(AuditEventClass::AuthenticationFailure, facts(401, None));
        }
        assert_eq!(
            sink.lines().len(),
            usize::try_from(AUTH_AUDIT_PER_SEC).unwrap_or(0)
        );
    }

    #[test]
    fn an_authentication_flood_from_one_tenant_does_not_silence_another() {
        // The window the class is rate-limited under is the tenant's own, so the
        // records of one tenant exhausting their budget leave the records of
        // every other tenant written.
        let rig = MetricsRig::build();
        let sink = sink();
        let telemetry = rig.telemetry(sink.clone());
        let class = AuditEventClass::AuthenticationFailure;
        let mut first = facts(401, None);
        first.tenant_id = Some("0b6c1a4e-2b0c-4d9f-9a1e-6f0f9b1f2a10".to_owned());
        let mut second = facts(401, None);
        second.tenant_id = Some("0c7d2b5f-3c1d-4e0a-0b2f-7a1a0c2a3b11".to_owned());
        for _ in 0..AUTH_AUDIT_PER_SEC {
            telemetry.audit(class.clone(), first.clone());
        }
        telemetry.audit(class.clone(), first);
        telemetry.audit(class, second);
        let lines = sink.lines();
        assert_eq!(
            lines.len(),
            usize::try_from(AUTH_AUDIT_PER_SEC).unwrap_or(0) + 1,
            "the first tenant is capped, the second is not"
        );
        assert!(
            lines[lines.len() - 1]
                .contains("\"tenant_id\":\"0c7d2b5f-3c1d-4e0a-0b2f-7a1a0c2a3b11\"")
        );
    }

    #[test]
    fn a_credential_carrying_record_is_suppressed_whole_and_never_written() {
        let rig = MetricsRig::build();
        let sink = sink();
        let telemetry = rig.telemetry(sink.clone());
        telemetry.audit(
            AuditEventClass::RequestFailure {
                passed_through_status: None,
            },
            AuditFacts {
                path: "/oagw/v1/proxy/payments/v1/pay?authorization=basic%20abc".to_owned(),
                ..facts(502, None)
            },
        );
        assert!(sink.lines().is_empty());
    }

    #[test]
    fn the_request_identifier_is_the_only_header_shaped_value_in_a_record() {
        let rig = MetricsRig::build();
        let sink = sink();
        let telemetry = rig.telemetry(sink.clone());
        telemetry.audit(
            AuditEventClass::RequestFailure {
                passed_through_status: None,
            },
            facts(502, None),
        );
        let lines = sink.lines();
        assert_eq!(lines.len(), 1);
        // The platform trace context is the only header-shaped value, and no
        // correlation header is added to the outbound request by this feature.
        assert!(lines[0].contains("\"request_id\":\"4bf92f3577b34da6a3ce929d0e0e4736\""));
        assert!(!lines[0].contains("authorization"));
        assert!(!lines[0].contains("cookie"));
    }

    #[test]
    fn the_record_of_a_passed_through_error_status_carries_no_error_type() {
        let rig = MetricsRig::build();
        let sink = sink();
        let telemetry = rig.telemetry(sink.clone());
        telemetry.audit(
            AuditEventClass::RequestFailure {
                passed_through_status: Some(500),
            },
            facts(500, None),
        );
        let line = &sink.lines()[0];
        assert!(line.contains("\"error_type\":null"));
        assert!(!line.contains("error_message"));
    }
}
