//! The in-process observation seam of `cpt-cf-oagw-feature-observability`.
//!
//! Three mechanisms live here, which is the whole of the feature's runtime
//! presence: the [`MetricsRegistry`] the twelve families of DESIGN §4.2 are
//! collected into and rendered from, the audit emitter that turns an
//! [`AuditEvent`] into one JSON line and hands it to an [`AuditSink`], and the
//! [`Observability`] facade that joins them and owns the two build-time
//! constants' state — the sampling decision is the correlation context's, and
//! the failure-log bound is this module's window.
//!
//! The seam is in-process because the single-executable deployment is what
//! makes it the only mechanism the feature has
//! (`cpt-cf-oagw-constraint-toolkit-deploy`): a scrape of `GET /oagw/v1/metrics`
//! reads the same process that served the traffic, and the audit stream is
//! written to the stdout that process already owns. No persisted state is
//! created, transitioned, or retained: a restart empties the counters, resets
//! the gauges, and loses no record that had not already been written.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use uuid::Uuid;

use crate::domain::error::ErrorKind;
use crate::domain::observability::{
    AuditEvent, CorrelationContext, MetricLabelSet, SamplingDecision, AUTH_FAILURE_LOG_INTERVAL_MS,
    AUTH_FAILURE_LOG_LIMIT, CORRELATION_HEADER, ERROR_TYPE_UPSTREAM, HISTOGRAM_BUCKETS,
    is_high_volume_pattern,
};
use crate::domain::proxy::ProxyResponse;
use crate::domain::ratelimit::{BreakerPhase, BreakerTransition};

/// The `host` value the three answer families carry for a request the gateway
/// answered without resolving an upstream.
///
/// Such a request addressed an alias the gateway names no configured upstream
/// for, so the alias it carries is one the caller invented, and filing the
/// answer under it would let a caller grow the label set without bound — the
/// outcome the cardinality rules of DESIGN §4.2 exist to prevent. The one
/// literal is not a hostname and cannot collide with a configured alias, which
/// `Alias::parse` normalizes, and `oagw_requests_in_flight` keeps the addressed
/// alias because its raise and its lower must name the same series.
pub const UNRESOLVED_HOST: &str = "_unresolved";

/// The families the exposition declares, with the type and the help line each
/// is rendered with.
///
/// The twelve rows are the twelve families DESIGN §4.2 enumerates, in the order
/// that section lists them, and no row outside them is declared: adding a
/// family no supplied document names is the one cardinality breach the
/// FEATURE's §5 forbids outright.
const FAMILIES: [Family; 12] = [
    Family {
        name: "oagw_requests_total",
        kind: Kind::Counter,
        help: "Proxy requests the gateway served, by upstream, method, route, and status.",
        exposed: true,
    },
    Family {
        name: "oagw_request_duration_seconds",
        kind: Kind::Histogram,
        help: "Request duration by phase, in seconds.",
        exposed: true,
    },
    Family {
        name: "oagw_requests_in_flight",
        kind: Kind::Gauge,
        help: "Proxy requests admitted and not yet finished, by upstream.",
        exposed: true,
    },
    Family {
        name: "oagw_errors_total",
        kind: Kind::Counter,
        help: "Failed requests, by upstream, route, and error type.",
        exposed: true,
    },
    Family {
        name: "oagw_circuit_breaker_state",
        kind: Kind::Gauge,
        help: "The phase the circuit breaker of an upstream holds.",
        exposed: true,
    },
    Family {
        name: "oagw_rate_limit_exceeded_total",
        kind: Kind::Counter,
        help: "Requests a rate limit refused, by upstream and route.",
        exposed: true,
    },
    Family {
        name: "oagw_circuit_breaker_transitions_total",
        kind: Kind::Counter,
        help: "Circuit-breaker transitions, by upstream and the two phases.",
        exposed: true,
    },
    Family {
        name: "oagw_rate_limit_usage_ratio",
        kind: Kind::Gauge,
        help: "The allowance ratio of the effective rate limit, from 0.0 to 1.0.",
        exposed: true,
    },
    Family {
        name: "oagw_routing_target_host_used",
        kind: Kind::Counter,
        help: "Requests that named their endpoint through the routing header.",
        exposed: true,
    },
    Family {
        name: "oagw_routing_endpoint_selected",
        kind: Kind::Counter,
        help: "Endpoint selections, by upstream, endpoint, and selection method.",
        exposed: true,
    },
    Family {
        name: "oagw_upstream_available",
        kind: Kind::Gauge,
        help: "Whether the breaker of an upstream admits an attempt: 1 when it does, 0 when it does not.",
        exposed: true,
    },
    Family {
        name: "oagw_upstream_connections",
        kind: Kind::Gauge,
        help: "The shared outbound client's connection-pool occupancy, by state.",
        exposed: false,
    },
];

/// The exposition kind of one family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Counter,
    Gauge,
    Histogram,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Counter => "counter",
            Kind::Gauge => "gauge",
            Kind::Histogram => "histogram",
        }
    }
}

/// One declared family: its name, its exposition kind, its help line, and
/// whether the gear exposes the state it describes.
///
/// A family whose underlying state is not exposed is declared — its label set
/// is part of `MetricLabelSet` — and omitted from the exposition, rather than
/// emitted as a constant the gear does not measure.
#[derive(Debug, Clone, Copy)]
struct Family {
    name: &'static str,
    kind: Kind,
    help: &'static str,
    exposed: bool,
}

/// One series of one family: the label pairs in the family's declared order.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SeriesKey {
    family: &'static str,
    labels: Vec<(&'static str, String)>,
}

impl SeriesKey {
    fn new(family: &'static str, labels: &[(&'static str, String)]) -> Self {
        Self {
            family,
            labels: labels.to_vec(),
        }
    }

    /// The series' label pairs reordered into the family's declared order.
    ///
    /// A label the family does not declare is a cardinality breach, and the
    /// series that carries one is rendered as none: the declaration is the
    /// checkable property the cardinality rules of the FEATURE's §5 rest on.
    fn ordered(&self) -> Option<Vec<(&'static str, String)>> {
        let declared = MetricLabelSet::labels_of(self.family)?;
        let mut ordered = Vec::with_capacity(declared.len());
        for key in declared {
            let value = self
                .labels
                .iter()
                .find(|(candidate, _)| candidate == key)
                .map(|(_, value)| value.clone())?;
            ordered.push((*key, value));
        }
        Some(ordered)
    }
}

/// The accumulated observation of one histogram series.
#[derive(Debug, Clone, Copy, Default)]
struct Histogram {
    buckets: [u64; HISTOGRAM_BUCKETS.len()],
    sum: f64,
    count: u64,
}

impl Histogram {
    fn observe(&mut self, value: f64) {
        for (index, bound) in HISTOGRAM_BUCKETS.iter().enumerate() {
            if value <= *bound {
                self.buckets[index] += 1;
            }
        }
        self.sum += value;
        self.count += 1;
    }
}

/// The collector the twelve families are observed into.
///
/// Every family is a map from its label-value combinations to its value, and a
/// family that has observed nothing holds no series, which is what the
/// renderer reports as a family with its type and help and no samples.
#[derive(Debug, Default)]
pub struct MetricsRegistry {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    counters: HashMap<SeriesKey, u64>,
    gauges: HashMap<SeriesKey, f64>,
    histograms: HashMap<SeriesKey, Histogram>,
}

impl MetricsRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Increments one counter series by one.
    pub fn increment(&self, family: &'static str, labels: &[(&'static str, String)]) {
        let mut inner = self.inner.lock();
        *inner
            .counters
            .entry(SeriesKey::new(family, labels))
            .or_default() += 1;
    }

    /// Adds a delta to one gauge series, which is the raise and the lower of
    /// the in-flight gauge.
    pub fn add(&self, family: &'static str, labels: &[(&'static str, String)], delta: f64) {
        let mut inner = self.inner.lock();
        *inner.gauges.entry(SeriesKey::new(family, labels)).or_default() += delta;
    }

    /// Sets one gauge series to a value, which is how a state another feature
    /// owns is reported.
    pub fn set(&self, family: &'static str, labels: &[(&'static str, String)], value: f64) {
        let mut inner = self.inner.lock();
        inner
            .gauges
            .insert(SeriesKey::new(family, labels), value);
    }

    /// Records one observation into a histogram series.
    pub fn observe(&self, family: &'static str, labels: &[(&'static str, String)], value: f64) {
        let mut inner = self.inner.lock();
        inner
            .histograms
            .entry(SeriesKey::new(family, labels))
            .or_default()
            .observe(value);
    }

    /// Renders the Prometheus text exposition of the twelve families.
    ///
    /// A family that has observed nothing is rendered with its type and help
    /// and no samples rather than omitted, so a scraper that reads the
    /// exposition sees the catalogue and not a moving subset of it. The label
    /// values are escaped as the text exposition format requires, and the
    /// series of a family are rendered in label order so a scrape is stable.
    #[must_use]
    pub fn render(&self) -> String {
        // @cpt-begin:cpt-cf-oagw-algo-metrics-render:p1:inst-amr-read
        // Each of the twelve collectors is read at the moment the scrape is
        // served, so a family describing state another feature owns reports
        // that state as it stands and not as it stood at the last observation.
        let inner = self.inner.lock();
        let mut out = String::new();
        // @cpt-end:cpt-cf-oagw-algo-metrics-render:p1:inst-amr-read
        for family in FAMILIES {
            if !family.exposed {
                continue;
            }
            // @cpt-begin:cpt-cf-oagw-algo-metrics-render:p1:inst-amr-headers
            // A `# HELP` line and a `# TYPE` line are emitted for each family,
            // declaring counter, gauge, or histogram as DESIGN §4.2 assigns,
            // and a family that has observed nothing is emitted with its type
            // and help and no samples rather than omitted.
            out.push_str("# HELP ");
            out.push_str(family.name);
            out.push(' ');
            out.push_str(&escape_help(family.help));
            out.push('\n');
            out.push_str("# TYPE ");
            out.push_str(family.name);
            out.push(' ');
            out.push_str(family.kind.label());
            out.push('\n');
            // @cpt-end:cpt-cf-oagw-algo-metrics-render:p1:inst-amr-headers
            let mut series: Vec<Series> = Vec::new();
            match family.kind {
                Kind::Counter => {
                    for (key, value) in &inner.counters {
                        if key.family != family.name {
                            continue;
                        }
                        if let Some(ordered) = key.ordered() {
                            series.push(Series {
                                labels: ordered,
                                suffix: "",
                                value: value.to_string(),
                            });
                        }
                    }
                }
                Kind::Gauge => {
                    for (key, value) in &inner.gauges {
                        if key.family != family.name {
                            continue;
                        }
                        if let Some(ordered) = key.ordered() {
                            series.push(Series {
                                labels: ordered,
                                suffix: "",
                                value: render_gauge(*value),
                            });
                        }
                    }
                }
                Kind::Histogram => {
                    // @cpt-begin:cpt-cf-oagw-algo-metrics-render:p1:inst-amr-histogram
                    // The histogram family is rendered as its `_bucket` series
                    // over the twelve buckets DESIGN §4.2 states, with the
                    // `le` label, plus its `_sum` and its `_count` series, and
                    // every other family as one series per label-value
                    // combination its set admits.
                    for (key, histogram) in &inner.histograms {
                        if key.family != family.name {
                            continue;
                        }
                        let Some(ordered) = key.ordered() else {
                            continue;
                        };
                        for (index, bound) in HISTOGRAM_BUCKETS.iter().enumerate() {
                            // `observe` already folds every observation into
                            // every bucket at or above it, so the stored count
                            // is the cumulative one the `le` label promises;
                            // folding it again here would double-count.
                            let mut labels = ordered.clone();
                            labels.push(("le", render_bound(*bound)));
                            series.push(Series {
                                labels,
                                suffix: "_bucket",
                                value: histogram.buckets[index].to_string(),
                            });
                        }
                        let mut labels = ordered.clone();
                        labels.push(("le", String::from("+Inf")));
                        series.push(Series {
                            labels,
                            suffix: "_bucket",
                            value: histogram.count.to_string(),
                        });
                        series.push(Series {
                            labels: ordered.clone(),
                            suffix: "_sum",
                            value: render_gauge(histogram.sum),
                        });
                        series.push(Series {
                            labels: ordered.clone(),
                            suffix: "_count",
                            value: histogram.count.to_string(),
                        });
                    }
                    // @cpt-end:cpt-cf-oagw-algo-metrics-render:p1:inst-amr-histogram
                }
            }
            series.sort_by(|left, right| {
                let left = format!("{}{}", left.suffix, render_labels(&left.labels));
                let right = format!("{}{}", right.suffix, render_labels(&right.labels));
                left.cmp(&right)
            });
            // @cpt-begin:cpt-cf-oagw-algo-metrics-render:p1:inst-amr-values
            // Every label value is rendered under the closed sets §1.5
            // records, so no value reaches the exposition that the cardinality
            // rules would exclude, and no label of any family carries a tenant
            // value.
            for series in series {
                out.push_str(family.name);
                out.push_str(series.suffix);
                out.push_str(&render_labels(&series.labels));
                out.push(' ');
                out.push_str(&series.value);
                out.push('\n');
            }
            // @cpt-end:cpt-cf-oagw-algo-metrics-render:p1:inst-amr-values
        }
        // @cpt-begin:cpt-cf-oagw-algo-metrics-render:p1:inst-amr-return
        // RETURN the rendered exposition; the caller answers it with the
        // content type the text exposition format names and writes no audit
        // record for the scrape.
        out
        // @cpt-end:cpt-cf-oagw-algo-metrics-render:p1:inst-amr-return
    }
}

/// One rendered series: its label pairs, the histogram suffix it carries, and
/// the value it reports.
struct Series {
    labels: Vec<(&'static str, String)>,
    suffix: &'static str,
    value: String,
}

/// Renders one label list as the text exposition format writes it.
fn render_labels(labels: &[(&'static str, String)]) -> String {
    if labels.is_empty() {
        return String::new();
    }
    let rendered: Vec<String> = labels
        .iter()
        .map(|(key, value)| format!("{key}=\"{}\"", escape_label(value)))
        .collect();
    format!("{{{}}}", rendered.join(","))
}

/// Renders a gauge value, with the whole and fractional forms the format
/// distinguishes.
fn render_gauge(value: f64) -> String {
    if value == value.trunc() && value.abs() < 1_000_000.0 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// Renders a histogram bucket bound as the format writes it.
fn render_bound(bound: f64) -> String {
    if bound == bound.trunc() {
        format!("{}", bound as i64)
    } else {
        format!("{bound}")
    }
}

/// Escapes a help line: backslash and newline, as the format requires.
fn escape_help(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\n', "\\n")
}

/// Escapes a label value: backslash, double quote, and newline.
fn escape_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// The destination an audit record is written to.
///
/// The stdout sink is the destination DESIGN §4.3 names and the one this
/// feature writes to; a test supplies its own, so no test observes another
/// test's stream.
pub trait AuditSink: Send + Sync {
    /// Writes one record, already serialized as one line without its newline.
    fn write(&self, record: &str);
}

/// The stdout sink, which is the destination DESIGN §4.3 names.
///
/// The line is written through a single locked write so the bytes of one
/// record are never interleaved with the bytes of another.
#[derive(Debug, Default)]
pub struct StdoutSink;

impl AuditSink for StdoutSink {
    fn write(&self, record: &str) {
        use std::io::Write;
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        let _ = writeln!(handle, "{record}");
        let _ = handle.flush();
    }
}

/// The test sink, which holds the records a test wrote.
///
/// A test owns its own instance, so no test observes another's stream.
#[derive(Debug, Default)]
pub struct CollectingSink {
    records: Mutex<Vec<String>>,
}

impl CollectingSink {
    /// Creates an empty sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The records written so far, oldest first.
    #[must_use]
    pub fn records(&self) -> Vec<String> {
        self.records.lock().clone()
    }

    /// The records written so far, parsed as JSON objects.
    ///
    /// # Errors
    ///
    /// Returns the parse error of the first record that is not one JSON
    /// object, which is the property the single-line rule states.
    pub fn parsed(&self) -> Result<Vec<serde_json::Value>, serde_json::Error> {
        self.records()
            .iter()
            .map(|record| serde_json::from_str(record))
            .collect()
    }
}

impl AuditSink for CollectingSink {
    fn write(&self, record: &str) {
        self.records.lock().push(String::from(record));
    }
}

/// The interval state of the failure-log bound.
#[derive(Debug)]
struct FloodWindow {
    interval_started: Instant,
    written: u32,
}

impl Default for FloodWindow {
    fn default() -> Self {
        Self {
            interval_started: Instant::now(),
            written: 0,
        }
    }
}

impl FloodWindow {
    /// Whether one more authentication-failure record may be written in the
    /// interval the window is counting.
    ///
    /// The records beyond the bound are dropped and not queued, so the answer
    /// is the whole of the decision and the caller holds nothing back.
    // @cpt-dod:cpt-cf-oagw-dod-obs-sampling:p1
    fn admits(&mut self, now: Instant) -> bool {
        if now
            .saturating_duration_since(self.interval_started)
            .as_millis()
            >= u128::from(AUTH_FAILURE_LOG_INTERVAL_MS)
        {
            self.interval_started = now;
            self.written = 0;
        }
        if self.written >= AUTH_FAILURE_LOG_LIMIT {
            return false;
        }
        self.written += 1;
        true
    }
}

/// The runtime the feature observes through.
///
/// One per process, held by [`crate::OagwState`] beside the state ADR 0006
/// assigns the Data Plane, so a scrape and the proxy path read the same
/// collectors.
pub struct Observability {
    registry: MetricsRegistry,
    sink: parking_lot::RwLock<Arc<dyn AuditSink>>,
    flood: Mutex<FloodWindow>,
}

impl std::fmt::Debug for Observability {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Observability")
            .field("registry", &self.registry)
            .field("flood", &self.flood)
            .finish_non_exhaustive()
    }
}

impl Default for Observability {
    fn default() -> Self {
        Self::new()
    }
}

impl Observability {
    /// Creates the runtime with the stdout sink.
    #[must_use]
    pub fn new() -> Self {
        Self::with_sink(Arc::new(StdoutSink))
    }

    /// Creates the runtime with the sink the caller supplies.
    #[must_use]
    pub fn with_sink(sink: Arc<dyn AuditSink>) -> Self {
        Self {
            registry: MetricsRegistry::new(),
            sink: parking_lot::RwLock::new(sink),
            flood: Mutex::new(FloodWindow::default()),
        }
    }

    /// Replaces the sink, returning the one it replaced.
    ///
    /// The swap is a test's way of owning its stream, and no record is held
    /// across the swap: whatever was written went to the sink that was current
    /// when it was written.
    pub fn swap_sink(&self, sink: Arc<dyn AuditSink>) -> Arc<dyn AuditSink> {
        std::mem::replace(&mut *self.sink.write(), sink)
    }

    /// The collector the twelve families are observed into.
    #[must_use]
    pub fn registry(&self) -> &MetricsRegistry {
        &self.registry
    }

    /// Renders the exposition the scrape is answered with.
    #[must_use]
    pub fn render(&self) -> String {
        self.registry.render()
    }

    /// Raises the in-flight gauge for one admitted request.
    ///
    /// The label is the alias the request addressed, which is the point at the
    /// path's entry where the value the `host` label carries exists; the lower
    /// at the exit uses the same value, so a request that never resolves a
    /// target still returns the gauge to its prior value.
    pub fn raise_in_flight(&self, host: &str) {
        self.registry.add(
            "oagw_requests_in_flight",
            &[(MetricLabelSet::HOST, String::from(host))],
            1.0,
        );
    }

    /// Lowers the in-flight gauge for one finished exchange.
    pub fn lower_in_flight(&self, host: &str) {
        self.registry.add(
            "oagw_requests_in_flight",
            &[(MetricLabelSet::HOST, String::from(host))],
            -1.0,
        );
    }

    /// Emits the audit record one event names, applying the gates of the
    /// emitter's algorithm and writing one line to the sink.
    ///
    /// `high_volume` is the classification of the route the record describes,
    /// which only a success record is sampled against; a failed request, a
    /// breaker transition, and a configuration change are never sampled, and
    /// an authentication-failure record is bounded instead.
    // @cpt-dod:cpt-cf-oagw-dod-obs-audit:p1
    pub fn emit(&self, event: AuditEvent, high_volume: bool, sampling: SamplingDecision) {
        // @cpt-begin:cpt-cf-oagw-algo-audit-emit:p1:inst-ae-event
        // The event name the record carries is the one the outcome selected,
        // which is one of the twelve literals the closed set holds; the caller
        // built the record with it and this routine writes nothing outside it.
        // @cpt-end:cpt-cf-oagw-algo-audit-emit:p1:inst-ae-event
        // @cpt-begin:cpt-cf-oagw-algo-audit-emit:p1:inst-ae-populate
        // The fields the event calls for were populated by the caller from the
        // execution context, the correlation context, and the sibling states,
        // and the unpopulated ones are omitted at serialization rather than
        // written null or empty.
        // @cpt-end:cpt-cf-oagw-algo-audit-emit:p1:inst-ae-populate
        // @cpt-begin:cpt-cf-oagw-algo-audit-emit:p1:inst-ae-redact
        // The redaction ran before the record was built: the fourteen-field
        // record admits no body, no query parameter, and no header value but
        // the correlation header's, and no credential material, no `cred://`
        // reference value, and no control character reaches any field it
        // carries.
        // @cpt-end:cpt-cf-oagw-algo-audit-emit:p1:inst-ae-redact
        // @cpt-begin:cpt-cf-oagw-algo-audit-emit:p1:inst-ae-level
        // The level was assigned by the mapping of §1.5 when the record was
        // built, and no record is written at DEBUG.
        // @cpt-end:cpt-cf-oagw-algo-audit-emit:p1:inst-ae-level
        // @cpt-begin:cpt-cf-oagw-algo-audit-emit:p1:inst-ae-sample-if
        // The success record of a high-volume route is the one record the
        // sampling decision governs, and the decision is the correlation
        // context's, read once per request.
        if event.event.as_deref() == Some(crate::domain::observability::EVENT_REQUEST_SUCCEEDED)
            && high_volume
            && sampling == SamplingDecision::Drop
        {
            return;
        }
        // @cpt-end:cpt-cf-oagw-algo-audit-emit:p1:inst-ae-sample-if
        // @cpt-begin:cpt-cf-oagw-algo-audit-emit:p1:inst-ae-sample
        // Apply the 1/100 decision of the correlation context and drop the
        // record when the decision is not to sample.
        // @cpt-end:cpt-cf-oagw-algo-audit-emit:p1:inst-ae-sample
        // @cpt-begin:cpt-cf-oagw-algo-audit-emit:p1:inst-ae-unbound
        // A failed request, a circuit-breaker transition, and a configuration
        // change reach this routine unsampled and unbound, because each is
        // either an event an operator must see or an event that is by
        // definition not high-volume: the caller passed the classification
        // that says so, and the gate above read only the success record of a
        // high-volume route.
        // @cpt-end:cpt-cf-oagw-algo-audit-emit:p1:inst-ae-unbound
        // @cpt-begin:cpt-cf-oagw-algo-audit-emit:p1:inst-ae-flood-if
        // The authentication-failure record is the one record the failure-log
        // bound governs, and the surplus is dropped and not queued.
        if event.event.as_deref() == Some(crate::domain::observability::EVENT_AUTH_FAILED)
            && !self.flood.lock().admits(Instant::now())
        {
            return;
        }
        // @cpt-end:cpt-cf-oagw-algo-audit-emit:p1:inst-ae-flood-if
        // @cpt-begin:cpt-cf-oagw-algo-audit-emit:p1:inst-ae-bound
        // Apply the failure-log bound and drop the record when the interval's
        // allowance is spent.
        // @cpt-end:cpt-cf-oagw-algo-audit-emit:p1:inst-ae-bound
        // @cpt-begin:cpt-cf-oagw-algo-audit-emit:p1:inst-ae-else
        // A failed request, a circuit-breaker transition, and a configuration
        // change are never sampled and never bound.
        // @cpt-end:cpt-cf-oagw-algo-audit-emit:p1:inst-ae-else
        // @cpt-begin:cpt-cf-oagw-algo-audit-emit:p1:inst-ae-write
        // The record is serialized as one JSON object with the fourteen field
        // names in the order DESIGN §4.3 lists them and written as one line.
        let record = serialize(&event);
        self.sink.read().write(&record);
        // @cpt-end:cpt-cf-oagw-algo-audit-emit:p1:inst-ae-write
        // @cpt-begin:cpt-cf-oagw-algo-audit-emit:p1:inst-ae-return
        // RETURN nothing: the routine produces no value the caller uses, holds
        // nothing after the write, and never revisits a record it wrote.
        // @cpt-end:cpt-cf-oagw-algo-audit-emit:p1:inst-ae-return
    }

    /// The count of authentication-failure records the current interval has
    /// admitted, which a test reads to prove the bound.
    #[must_use]
    pub fn auth_failures_written(&self) -> u32 {
        self.flood.lock().written
    }

    /// Emits the configuration-change record one completed management write
    /// produced.
    ///
    /// The event name carries the resource kind and the operation, the writer's
    /// tenant and subject are the identity fields, and the path and method are
    /// the management path addressed: `host`, `duration_ms`, `request_size`,
    /// and `response_size` are omitted, because no proxy exchange happened.
    /// A configuration change is by definition not high-volume, so the record
    /// is never sampled.
    pub fn config_change(
        &self,
        event_name: &'static str,
        tenant_id: Option<uuid::Uuid>,
        principal_id: Option<String>,
        method: &str,
        path: &str,
        status: u16,
    ) {
        let event = AuditEvent {
            timestamp: Some(timestamp_of(SystemTime::now())),
            level: Some(String::from("INFO")),
            event: Some(String::from(event_name)),
            request_id: None,
            tenant_id: tenant_id.map(|tenant| tenant.to_string()),
            principal_id,
            host: None,
            path: Some(String::from(path)),
            method: Some(String::from(method)),
            status: Some(status),
            duration_ms: None,
            request_size: None,
            response_size: None,
            error_type: None,
        };
        self.emit(event, false, SamplingDecision::Keep);
    }
}

/// The redaction the emitter applies before any field is serialized.
///
/// The fourteen-field record admits no body, no query parameter, and no header
/// value other than the correlation header's, and the `path` field is the one
/// a caller-controlled string could reach a query through, so it is truncated
/// at the query marker; a field that carries a `cred://` reference value, a
/// bearer token, or a control character is dropped rather than written,
/// because a value that cannot be written safely has no value for the record.
// @cpt-dod:cpt-cf-oagw-dod-obs-redaction:p1
fn redact(name: &'static str, value: String) -> Option<String> {
    let value = if name == "path" {
        match value.split_once('?') {
            Some((prefix, _)) => String::from(prefix),
            None => value,
        }
    } else {
        value
    };
    if value.contains("cred://") || value.contains("Bearer ") {
        return None;
    }
    if value.bytes().any(|byte| byte < 0x20 || byte == 0x7F) {
        return None;
    }
    Some(value)
}

/// Serializes one record as one JSON object, with the fourteen field names in
/// the order DESIGN §4.3 lists them and no fifteenth member.
///
/// A field with no value is omitted rather than written null or empty, which
/// is the omission rule the record's own type states.
#[must_use]
pub fn serialize(event: &AuditEvent) -> String {
    let mut out = String::from("{");
    let mut first = true;
    for (name, value) in event.populated() {
        let Some(value) = redact(name, value) else {
            continue;
        };
        if !first {
            out.push(',');
        }
        first = false;
        out.push('"');
        out.push_str(name);
        out.push_str("\":\"");
        out.push_str(&escape_json(&value));
        out.push('"');
    }
    out.push('}');
    out
}

/// Escapes a field value as a JSON string literal.
fn escape_json(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            character if (character as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", character as u32));
            }
            character => out.push(character),
        }
    }
    out
}

/// The instant a record is issued at, read once and formatted as RFC 3339 in
/// UTC.
///
/// The format is the one a JSON log consumer reads and the record carries no
/// timezone of its own, because the record's instant is the instant the write
/// was issued at and nothing else.
#[must_use]
pub fn timestamp_of(at: SystemTime) -> String {
    let since = at
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::from_secs(0));
    let (days, seconds) = (since.as_secs() / 86_400, since.as_secs() % 86_400);
    let (year, month, day) = civil_from_days(i64::try_from(days).unwrap_or(0));
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds / 3_600,
        (seconds % 3_600) / 60,
        seconds % 60
    )
}

/// The civil date of a day count from the epoch, which the timestamp reads.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_prime + 2) / 5 + 1;
    let month = if shifted_prime < 10 {
        shifted_prime + 3
    } else {
        shifted_prime - 9
    };
    (if month <= 2 { year + 1 } else { year }, month as u32, day as u32)
}

/// The phase timings the proxy path stamps as it goes, which the duration
/// family reads.
///
/// The four phases are the bounded set whose durations the request's execution
/// context carries, and no per-plugin, per-route, or per-upstream phase value
/// is ever recorded.
#[derive(Debug, Clone, Copy)]
pub struct PhaseTimings {
    entry: Instant,
    resolve: Option<Instant>,
    chain: Option<Instant>,
    upstream: Option<Instant>,
}

impl PhaseTimings {
    /// Starts the timings at the path's entry.
    #[must_use]
    pub fn started() -> Self {
        Self {
            entry: Instant::now(),
            resolve: None,
            chain: None,
            upstream: None,
        }
    }

    /// Stamps the moment the resolution completed.
    pub fn resolved(&mut self) {
        self.resolve = Some(Instant::now());
    }

    /// Stamps the moment the composed chain completed.
    pub fn chained(&mut self) {
        self.chain = Some(Instant::now());
    }

    /// Stamps the moment the outbound forward completed.
    pub fn forwarded(&mut self) {
        self.upstream = Some(Instant::now());
    }

    /// The durations of the phases that completed, in the order the path runs
    /// them, plus `total` measured to the moment the read is made.
    ///
    /// A phase is the span from its predecessor's stamp to its own, so the
    /// four numbers sum to the whole of the request and none of them restates
    /// another's: `resolve` runs from the entry to the resolution's completion,
    /// `chain` from there to the composed chain's, and `upstream` from there to
    /// the forward's. A phase the path never reached is absent rather than
    /// reported as a zero-length span, which is what keeps the per-phase mean
    /// an operator reads free of requests that touched no upstream.
    #[must_use]
    pub fn durations(&self) -> Vec<(&'static str, f64)> {
        let at = Instant::now();
        let mut phases = Vec::with_capacity(4);
        for (name, from, to) in [
            ("resolve", Some(self.entry), self.resolve),
            ("chain", self.resolve, self.chain),
            ("upstream", self.chain, self.upstream),
        ] {
            if let Some(seconds) = self.span(from, to) {
                phases.push((name, seconds));
            }
        }
        phases.push(("total", at.saturating_duration_since(self.entry).as_secs_f64()));
        phases
    }

    /// The seconds a phase ran for, which is `None` when the phase never
    /// completed and the phase before it never completed either.
    fn span(&self, from: Option<Instant>, to: Option<Instant>) -> Option<f64> {
        let to = to?;
        let from = from.unwrap_or(self.entry);
        Some(to.saturating_duration_since(from).as_secs_f64())
    }

    /// The seconds the whole request took, which is the `total` phase.
    #[must_use]
    pub fn total_seconds(&self) -> f64 {
        Instant::now().saturating_duration_since(self.entry).as_secs_f64()
    }
}

/// The endpoint selection the proxy path performed, which the routing families
/// report.
#[derive(Debug, Clone)]
pub struct EndpointObservation {
    /// The upstream the selection selected from.
    pub upstream_id: Uuid,
    /// The endpoint host the selection named.
    pub endpoint_host: String,
    /// How the selection chose it.
    pub method: &'static str,
    /// Whether the request named its target through the routing header.
    pub used_header: bool,
}

/// The breaker state and transitions the rate-limiting feature's machine
/// reported, read without mutating it.
#[derive(Debug, Clone, Default)]
pub struct BreakerObservation {
    /// The phase the machine holds at the exit.
    pub phase: Option<BreakerPhase>,
    /// The transitions the machine reported over the exchange.
    pub transitions: Vec<BreakerTransition>,
}

/// The rate-limit outcome the check produced, read without mutating the
/// machine that produced it.
#[derive(Debug, Clone, Copy, Default)]
pub struct RateLimitObservation {
    /// Whether the check refused the request.
    pub exceeded: bool,
    /// The allowance ratio the check computed, when it computed one.
    pub usage_ratio: Option<f64>,
}

/// The exchange the proxy path served, as the exit step of the observed flow
/// reads it.
///
/// Every member is a value the path already computed: the observation adds no
/// computation of its own beyond the reading of them.
#[derive(Debug, Clone, Default)]
pub struct Exchange {
    /// The resolved upstream's alias, which is the `host` label and the audit
    /// `host` field.
    pub host: Option<String>,
    /// The matched route's normalized match pattern, which is the `http.route`
    /// label and the audit `path` field.
    pub route: Option<String>,
    /// The request method as issued.
    pub method: String,
    /// The status the caller was answered with.
    pub status: Option<u16>,
    /// The catalogue failure the gateway answered with, when it did.
    pub error: Option<ErrorKind>,
    /// The phase timings the path stamped.
    pub timings: Option<PhaseTimings>,
    /// The request bytes as transferred.
    pub request_size: u64,
    /// The response bytes as transferred, which a streamed exchange defers to
    /// its transfer's end.
    pub response_size: Option<u64>,
    /// The endpoint selection, when one ran.
    pub endpoint: Option<EndpointObservation>,
    /// The breaker observation, when a machine was consulted.
    pub breaker: Option<BreakerObservation>,
    /// The rate-limit outcome, when a limit was in force.
    pub rate_limit: Option<RateLimitObservation>,
    /// The streamed session whose end defers the whole observation.
    pub session: Option<Arc<Mutex<crate::domain::stream::StreamSession>>>,
    /// Whether the exchange's answer was produced by the gateway, which is
    /// what decides whether a `trace_id` echo is carried.
    pub gateway_answer: bool,
    /// Whether the answer is the one the proxy path gives a caller whose
    /// identity or permission it could not establish, which §1.5 row 177 names
    /// as an authentication failure this feature records under `auth.failed`.
    pub authentication_failure: bool,
    /// Whether the path resolved a configured upstream before it answered,
    /// which decides whether the answer is filed under the resolved alias or
    /// under [`UNRESOLVED_HOST`].
    pub upstream_resolved: bool,
}

impl Exchange {
    /// Whether the answer the caller received is one the observed flow records
    /// as failed: a gateway error, or an upstream answer with a failure status.
    #[must_use]
    pub fn failed(&self) -> bool {
        self.error.is_some()
            || self.status.is_some_and(|status| status >= 400)
    }

    /// The `error_type` value the record and the error family carry.
    ///
    /// A gateway error carries its catalogue row's slug; an upstream failure
    /// status carries the one literal the catalogue has no row for; a
    /// successful answer carries none. A gateway answer that names no
    /// catalogue variant — a CORS refusal, a 401, a 403 the enforcer
    /// produced — carries none either, because `error_type` is closed at the
    /// catalogue's slugs plus `upstream` and a status the gateway produced
    /// itself is neither.
    #[must_use]
    pub fn error_type(&self) -> Option<&'static str> {
        if let Some(kind) = self.error {
            return crate::domain::observability::error_slug_of(kind.gts_type());
        }
        if !self.gateway_answer && self.status.is_some_and(|status| status >= 400) {
            return Some(ERROR_TYPE_UPSTREAM);
        }
        None
    }

    /// Whether the route the exchange served is one the sampling ratio
    /// governs.
    #[must_use]
    pub fn high_volume(&self) -> bool {
        self.route
            .as_deref()
            .is_some_and(is_high_volume_pattern)
    }
}

impl Observability {
    /// Observes one finished exchange and emits its record.
    ///
    /// This is the exit step of `cpt-cf-oagw-flow-request-observed`: the
    /// metric families are updated from the execution context and the sibling
    /// states, and the audit record is written once. A streamed exchange is
    /// observed by its [`DeferredObservation`] instead, which carries the same
    /// exchange to its transfer's end.
    pub fn observe(&self, exchange: &Exchange, correlation: Option<&CorrelationContext>) {
        // @cpt-begin:cpt-cf-oagw-algo-metrics-observe:p1:inst-amo-return
        let Some(host) = exchange.host.clone() else {
            return;
        };
        // @cpt-end:cpt-cf-oagw-algo-metrics-observe:p1:inst-amo-return
        // @cpt-begin:cpt-cf-oagw-algo-metrics-observe:p1:inst-amo-labels
        // The label values are derived under the rules of §1.5: the host is
        // the resolved upstream's alias, the route is the matched route's
        // normalized match pattern and never the raw request path, the method
        // is the standard verb or `_OTHER`, and the status is the numeric code
        // the caller received.
        //
        // A request the gateway answered without resolving an upstream — a
        // refused or unmatched alias — names no upstream, so its answer is
        // filed under the one bounded literal rather than under the alias the
        // caller invented, which is what keeps the label set of the three
        // answer families out of caller control. The in-flight gauge is the
        // one family that keeps the addressed alias, because its raise at the
        // correlate step and its lower here must name the same series.
        let answer_host = if exchange.upstream_resolved {
            host.clone()
        } else {
            String::from(UNRESOLVED_HOST)
        };
        let route = exchange.route.clone();
        let method = crate::domain::observability::normalize_method(&exchange.method);
        let status = exchange.status.unwrap_or_default().to_string();
        let labels = [
            (MetricLabelSet::HOST, answer_host.clone()),
            (MetricLabelSet::HTTP_METHOD, String::from(method)),
            (
                MetricLabelSet::HTTP_ROUTE,
                route.clone().unwrap_or_default(),
            ),
            (MetricLabelSet::HTTP_STATUS, status),
        ];
        // @cpt-end:cpt-cf-oagw-algo-metrics-observe:p1:inst-amo-labels
        // @cpt-begin:cpt-cf-oagw-algo-metrics-observe:p1:inst-amo-requests
        // The request is counted once, with the four labels of its set, and
        // the status carried is the one the caller received, whatever produced
        // it.
        self.registry.increment("oagw_requests_total", &labels);
        // @cpt-end:cpt-cf-oagw-algo-metrics-observe:p1:inst-amo-requests
        // @cpt-begin:cpt-cf-oagw-algo-metrics-observe:p1:inst-amo-duration
        // The four phases the execution context carries are the only phases
        // observed.
        if let Some(timings) = exchange.timings {
            for (phase, seconds) in timings.durations() {
                let labels = vec![
                    (MetricLabelSet::HOST, answer_host.clone()),
                    (
                        MetricLabelSet::HTTP_ROUTE,
                        route.clone().unwrap_or_default(),
                    ),
                    (MetricLabelSet::PHASE, String::from(phase)),
                ];
                self.registry
                    .observe("oagw_request_duration_seconds", &labels, seconds);
            }
        }
        // @cpt-end:cpt-cf-oagw-algo-metrics-observe:p1:inst-amo-duration
        // @cpt-begin:cpt-cf-oagw-algo-metrics-observe:p1:inst-amo-inflight
        // The lower half of the raise the correlate step performed.
        self.lower_in_flight(&host);
        // @cpt-end:cpt-cf-oagw-algo-metrics-observe:p1:inst-amo-inflight
        // @cpt-begin:cpt-cf-oagw-algo-metrics-observe:p1:inst-amo-error-if
        // A gateway error carries its catalogue slug, an upstream failure
        // status carries the `upstream` literal, and a bare refusal carries
        // neither.
        if let Some(error_type) = exchange.error_type() {
            // @cpt-begin:cpt-cf-oagw-algo-metrics-observe:p1:inst-amo-error
            // The error is counted with the host, the route, and the type of
            // the failure, and a successful answer increments nothing in this
            // family.
            self.registry.increment(
                "oagw_errors_total",
                &[
                    (MetricLabelSet::HOST, answer_host),
                    (MetricLabelSet::HTTP_ROUTE, route.clone().unwrap_or_default()),
                    (MetricLabelSet::ERROR_TYPE, String::from(error_type)),
                ],
            );
            // @cpt-end:cpt-cf-oagw-algo-metrics-observe:p1:inst-amo-error
        }
        // @cpt-end:cpt-cf-oagw-algo-metrics-observe:p1:inst-amo-error-if
        // @cpt-begin:cpt-cf-oagw-algo-metrics-observe:p1:inst-amo-error-else
        // @cpt-begin:cpt-cf-oagw-algo-metrics-observe:p1:inst-amo-no-error
        // Nothing is incremented in that family: a successful request is not
        // an error and no series of this feature reports success as one.
        // @cpt-end:cpt-cf-oagw-algo-metrics-observe:p1:inst-amo-no-error
        // @cpt-end:cpt-cf-oagw-algo-metrics-observe:p1:inst-amo-error-else
        self.observe_siblings(exchange, &host, &route, correlation);
        self.emit_record(exchange, correlation, &host, &route);
    }

    /// Reads the state the sibling features own into the six series that
    /// report it, without mutating any of it.
    fn observe_siblings(
        &self,
        exchange: &Exchange,
        host: &str,
        route: &Option<String>,
        correlation: Option<&CorrelationContext>,
    ) {
        // @cpt-begin:cpt-cf-oagw-algo-metrics-observe:p1:inst-amo-ratelimit
        // The breaker machine is read, never driven: the phase it holds, the
        // transitions it reported, the refusals it produced, and the allowance
        // ratio it computed are all read here.
        if let Some(breaker) = &exchange.breaker {
            if let Some(phase) = breaker.phase {
                self.registry.set(
                    "oagw_circuit_breaker_state",
                    &[(MetricLabelSet::HOST, String::from(host))],
                    breaker_gauge(phase),
                );
                if let Some(endpoint_host) = exchange
                    .endpoint
                    .as_ref()
                    .map(|endpoint| endpoint.endpoint_host.clone())
                {
                    self.registry.set(
                        "oagw_upstream_available",
                        &[
                            (MetricLabelSet::HOST, String::from(host)),
                            (MetricLabelSet::ENDPOINT, endpoint_host),
                        ],
                        f64::from(u8::from(admits(phase))),
                    );
                }
            }
            for transition in &breaker.transitions {
                self.registry.increment(
                    "oagw_circuit_breaker_transitions_total",
                    &[
                        (MetricLabelSet::HOST, String::from(host)),
                        (
                            MetricLabelSet::FROM_STATE,
                            String::from(crate::domain::observability::breaker_state_label(
                                transition.from,
                            )),
                        ),
                        (
                            MetricLabelSet::TO_STATE,
                            String::from(crate::domain::observability::breaker_state_label(
                                transition.to,
                            )),
                        ),
                    ],
                );
                // The transition's own record: the series above carries the
                // two states as its labels, and the record is the line an
                // operator who is not scraping sees. It is written in addition
                // to the one record the request produces, never instead of it,
                // and it is never sampled.
                self.emit(
                    AuditEvent {
                        timestamp: Some(timestamp_of(SystemTime::now())),
                        level: Some(String::from(if transition.to == BreakerPhase::Open {
                            "WARN"
                        } else {
                            "INFO"
                        })),
                        event: Some(String::from(
                            crate::domain::observability::EVENT_BREAKER_TRANSITIONED,
                        )),
                        request_id: correlation.map(|correlation| correlation.request_id.clone()),
                        tenant_id: correlation
                            .and_then(|correlation| correlation.tenant_id.map(|t| t.to_string())),
                        principal_id: correlation
                            .and_then(|correlation| correlation.principal_id.clone()),
                        host: Some(String::from(host)),
                        path: route.clone(),
                        method: Some(exchange.method.clone()),
                        status: None,
                        duration_ms: None,
                        request_size: None,
                        response_size: None,
                        error_type: None,
                    },
                    false,
                    SamplingDecision::Keep,
                );
            }
        }
        if let Some(rate_limit) = exchange.rate_limit {
            if rate_limit.exceeded {
                self.registry.increment(
                    "oagw_rate_limit_exceeded_total",
                    &[
                        (MetricLabelSet::HOST, String::from(host)),
                        (
                            MetricLabelSet::PATH,
                            route.clone().unwrap_or_default(),
                        ),
                    ],
                );
            }
            if let Some(ratio) = rate_limit.usage_ratio {
                self.registry.set(
                    "oagw_rate_limit_usage_ratio",
                    &[
                        (MetricLabelSet::HOST, String::from(host)),
                        (MetricLabelSet::PATH, route.clone().unwrap_or_default()),
                    ],
                    ratio,
                );
            }
        }
        // @cpt-end:cpt-cf-oagw-algo-metrics-observe:p1:inst-amo-ratelimit
        // @cpt-begin:cpt-cf-oagw-algo-metrics-observe:p1:inst-amo-routing
        // The selection the proxy path performed is reported with the method
        // it recorded, and the routing header's use with it.
        if let Some(endpoint) = &exchange.endpoint {
            self.registry.increment(
                "oagw_routing_endpoint_selected",
                &[
                    (
                        MetricLabelSet::UPSTREAM_ID,
                        endpoint.upstream_id.to_string(),
                    ),
                    (
                        MetricLabelSet::ENDPOINT_HOST,
                        endpoint.endpoint_host.clone(),
                    ),
                    (
                        MetricLabelSet::SELECTION_METHOD,
                        String::from(endpoint.method),
                    ),
                ],
            );
            if endpoint.used_header {
                self.registry.increment(
                    "oagw_routing_target_host_used",
                    &[
                        (
                            MetricLabelSet::UPSTREAM_ID,
                            endpoint.upstream_id.to_string(),
                        ),
                        (
                            MetricLabelSet::ENDPOINT_HOST,
                            endpoint.endpoint_host.clone(),
                        ),
                    ],
                );
            }
        }
        // @cpt-end:cpt-cf-oagw-algo-metrics-observe:p1:inst-amo-routing
        // @cpt-begin:cpt-cf-oagw-algo-metrics-observe:p1:inst-amo-health
        // The connection-pool occupancy is not exposed by the shared outbound
        // client, so the family that reports it is rendered with its type and
        // help and no samples rather than as a constant, and nothing is
        // observed into it here.
        // @cpt-end:cpt-cf-oagw-algo-metrics-observe:p1:inst-amo-health
        // @cpt-begin:cpt-cf-oagw-algo-metrics-observe:p1:inst-amo-no-tenant
        // No label above carries a tenant value, which is the one rule of the
        // cardinality management that has no exception.
        // @cpt-end:cpt-cf-oagw-algo-metrics-observe:p1:inst-amo-no-tenant
    }

    /// Builds, gates, and writes the record the exchange produces.
    fn emit_record(
        &self,
        exchange: &Exchange,
        correlation: Option<&CorrelationContext>,
        host: &str,
        route: &Option<String>,
    ) {
        // @cpt-begin:cpt-cf-oagw-flow-request-observed:p1:inst-ro-emit
        // The record is built from the execution context, the correlation
        // context, and the sibling states, and is written once.
        // @cpt-begin:cpt-cf-oagw-flow-request-observed:p1:inst-ro-failed-if
        // The level, the event literal, and the error type are the three
        // fields the failure of the exchange decides.
        let (level, event_name, error_type) = if exchange.failed() {
            // An upstream failure status is a failure of the kind §1.5
            // assigns ERROR to: the upstream answered the caller with a status
            // that is not an answer, and the gateway carries no catalogue
            // variant for it.
            let upstream_failure = exchange.error.is_none()
                && !exchange.gateway_answer
                && exchange.status.is_some_and(|status| status >= 400);
            let level = if upstream_failure || exchange.authentication_failure {
                "ERROR"
            } else {
                crate::domain::observability::request_event_of(true, exchange.error).1
            };
            // The authorization refusal the proxy path answers — a caller whose
            // identity or whose permission it could not establish — is the
            // authentication failure §1.5 row 177 names, so it is carried under
            // the `auth.failed` literal at the ERROR level the mapping assigns
            // it and the failure-log bound of §1.5 governs it.
            let event_name = if exchange.authentication_failure
                || matches!(
                    exchange.error,
                    Some(
                        crate::domain::error::ErrorKind::AuthenticationFailed
                            | crate::domain::error::ErrorKind::SecretNotFound
                    )
                ) {
                crate::domain::observability::EVENT_AUTH_FAILED
            } else {
                crate::domain::observability::request_event_of(true, exchange.error).0
            };
            // @cpt-begin:cpt-cf-oagw-flow-request-observed:p1:inst-ro-failed-record
            // The record is a failed record: its level is the one the mapping
            // assigns — ERROR for an upstream failure, a timeout, and an
            // authentication failure, WARN for a rate-limit refusal and a
            // breaker-open answer, and INFO for every other refusal — its
            // `error_type` is the catalogue slug of the variant the gateway
            // answered or `upstream` for an upstream failure status, and every
            // field a success record carries is present alongside it.
            (
                level,
                event_name,
                exchange.error_type().map(String::from),
            )
            // @cpt-end:cpt-cf-oagw-flow-request-observed:p1:inst-ro-failed-record
        // @cpt-end:cpt-cf-oagw-flow-request-observed:p1:inst-ro-failed-if
        // @cpt-begin:cpt-cf-oagw-flow-request-observed:p1:inst-ro-failed-else
        } else {
            // @cpt-begin:cpt-cf-oagw-flow-request-observed:p1:inst-ro-success-record
            // The record is a success record at INFO, its `error_type` is
            // omitted, and its sampling decision is the high-volume-route
            // decision §1.5 records.
            ("INFO", "proxy_request.succeeded", None)
            // @cpt-end:cpt-cf-oagw-flow-request-observed:p1:inst-ro-success-record
        };
        // @cpt-end:cpt-cf-oagw-flow-request-observed:p1:inst-ro-failed-else
        let event = AuditEvent {
            timestamp: Some(timestamp_of(SystemTime::now())),
            request_id: correlation.map(|correlation| correlation.request_id.clone()),
            tenant_id: correlation
                .and_then(|correlation| correlation.tenant_id.map(|tenant| tenant.to_string())),
            principal_id: correlation.and_then(|correlation| correlation.principal_id.clone()),
            host: Some(String::from(host)),
            path: route.clone(),
            method: Some(exchange.method.clone()),
            status: exchange.status,
            duration_ms: exchange.timings.map(|timings| {
                u64::try_from((timings.total_seconds() * 1_000.0).round() as i64)
                    .unwrap_or_default()
            }),
            request_size: Some(exchange.request_size),
            response_size: exchange.response_size,
            level: Some(String::from(level)),
            event: Some(String::from(event_name)),
            error_type,
        };
        let high_volume = exchange.high_volume();
        let sampling = correlation.map_or(SamplingDecision::Drop, |correlation| {
            correlation.sampling
        });
        self.emit(event, high_volume, sampling);
        // @cpt-end:cpt-cf-oagw-flow-request-observed:p1:inst-ro-emit
    }

    /// Installs the deferred observation of a streamed exchange.
    ///
    /// The observation runs when the transfer's session ends, so the in-flight
    /// gauge stays raised for the whole of the transfer and the record carries
    /// the byte counts as transferred.
    #[must_use]
    pub fn defer(
        self: Arc<Self>,
        exchange: Exchange,
        correlation: Option<CorrelationContext>,
    ) -> DeferredObservation {
        DeferredObservation {
            observability: self,
            exchange: Some(exchange),
            correlation,
        }
    }
}

/// The deferred observation of a streamed exchange, which runs when the
/// transfer's session ends.
///
/// The guard holds the exchange the handler assembled and the correlation
/// context the request carries; when it is dropped the observation reads the
/// session's final state and emits the record the exchange produces. It owns
/// its seam, because the tunnel of a taken-up upgrade outlives the handler
/// that answered its 101 and the guard travels into the task the tunnel runs
/// in.
#[derive(Debug)]
pub struct DeferredObservation {
    observability: Arc<Observability>,
    exchange: Option<Exchange>,
    correlation: Option<CorrelationContext>,
}

impl DeferredObservation {
    /// Runs the observation now, reading whatever the session recorded.
    fn run(&mut self) {
        let Some(mut exchange) = self.exchange.take() else {
            return;
        };
        if let Some(session) = &exchange.session {
            let session = session.lock();
            exchange.response_size = Some(session.moved);
        }
        self.observability
            .observe(&exchange, self.correlation.as_ref());
    }
}

impl Drop for DeferredObservation {
    fn drop(&mut self) {
        self.run();
    }
}

/// The gauge value a breaker phase is reported under, as the state family
/// enumerates it.
fn breaker_gauge(phase: BreakerPhase) -> f64 {
    match phase {
        BreakerPhase::Closed => 0.0,
        BreakerPhase::Open => 1.0,
        BreakerPhase::HalfOpen => 2.0,
    }
}

/// Whether a breaker phase admits an attempt, which is what the availability
/// gauge reports as 1 and 0.
fn admits(phase: BreakerPhase) -> bool {
    matches!(phase, BreakerPhase::Closed | BreakerPhase::HalfOpen)
}

/// The `ProxyResponse` classification the exit reads, kept here so the exit
/// step of the observed flow and the classification of the proxy path cannot
/// disagree about what an upstream answer is.
#[must_use]
pub fn source_of(response: &ProxyResponse) -> &'static str {
    match response.source {
        crate::domain::error::ErrorSource::Gateway => "gateway",
        crate::domain::error::ErrorSource::Upstream => "upstream",
    }
}

/// The correlation header's value as the request carried it, which is the one
/// header value a record may carry.
#[must_use]
pub fn correlation_header(headers: &[(String, String)]) -> Option<&str> {
    let lower = CORRELATION_HEADER;
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(lower))
        .map(|(_, value)| value.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::observability::{AUDIT_FIELDS, CorrelationContext, PHASES};

    fn event(name: &str) -> AuditEvent {
        AuditEvent {
            timestamp: Some(String::from("2026-01-01T00:00:00Z")),
            level: Some(String::from("INFO")),
            event: Some(String::from(name)),
            request_id: Some(String::from("req-1")),
            ..AuditEvent::default()
        }
    }

    #[test]
    fn serializes_in_the_tabulated_order() {
        // A fully populated record: the emitted key sequence is the tabulated
        // order itself, walked from `AUDIT_FIELDS`, so a field that drifted to
        // another position fails here rather than passing under a five-way
        // spot check.
        let record = AuditEvent {
            timestamp: Some(String::from("2026-09-08T00:00:00Z")),
            level: Some(String::from("INFO")),
            event: Some(String::from(crate::domain::observability::EVENT_REQUEST_SUCCEEDED)),
            request_id: Some(String::from("req-1")),
            tenant_id: Some(String::from("tenant-1")),
            principal_id: Some(String::from("subject-1")),
            host: Some(String::from("api.example.com")),
            path: Some(String::from("/v1/things")),
            method: Some(String::from("GET")),
            status: Some(200),
            duration_ms: Some(12),
            request_size: Some(10),
            response_size: Some(20),
            error_type: Some(String::from(ERROR_TYPE_UPSTREAM)),
        };
        let line = serialize(&record);
        // Walking the tabulated names forward and requiring each to appear
        // after the last one found is the order check: a field that drifted to
        // an earlier position is not found after the cursor and fails here.
        let mut cursor = 0_usize;
        for name in AUDIT_FIELDS {
            let needle = format!("\"{name}\":");
            let at = line[cursor..].find(&needle).expect(name);
            cursor += at + needle.len();
        }
        assert!(!line.contains("null"));
        assert!(!line.contains("\"\""));
    }

    #[test]
    fn omits_an_unpopulated_field_and_never_writes_null() {
        let line = serialize(&event("proxy_request.succeeded"));
        assert!(!line.contains("error_type"));
        assert!(!line.contains("null"));
        assert!(!line.contains("\"host\":\"\""));
    }

    #[test]
    fn redacts_a_query_out_of_the_path() {
        let mut record = event("proxy_request.succeeded");
        record.path = Some(String::from("/v1/things?secret=1"));
        let line = serialize(&record);
        assert!(line.contains("\"path\":\"/v1/things\""));
        assert!(!line.contains("secret"));
    }

    #[test]
    fn redacts_a_credential_reference_and_a_token() {
        let mut record = event("config.upstream.created");
        record.path = Some(String::from("cred://store/key"));
        assert!(!serialize(&record).contains("cred://"));
        record.path = Some(String::from("Bearer abc"));
        assert!(!serialize(&record).contains("Bearer"));
    }

    #[test]
    fn escapes_a_label_and_a_field_value() {
        let registry = MetricsRegistry::new();
        registry.increment(
            "oagw_requests_total",
            &[
                (MetricLabelSet::HOST, String::from("host\"with\\quotes")),
                (MetricLabelSet::HTTP_METHOD, String::from("GET")),
                (MetricLabelSet::HTTP_ROUTE, String::from("/v1/things")),
                (MetricLabelSet::HTTP_STATUS, String::from("200")),
            ],
        );
        let rendered = registry.render();
        let expected = format!("host=\"{}\"", escape_label("host\"with\\quotes"));
        assert!(rendered.contains(&expected), "{rendered}");
    }

    #[test]
    fn renders_every_exposed_family_with_its_type_and_help() {
        let registry = MetricsRegistry::new();
        let rendered = registry.render();
        for family in FAMILIES {
            if !family.exposed {
                // A family whose underlying state is not exposed is omitted
                // rather than emitted as a constant.
                assert!(!rendered.contains(family.name));
                continue;
            }
            assert!(rendered.contains(&format!("# HELP {}", family.name)));
            assert!(
                rendered.contains(&format!("# TYPE {} {}", family.name, family.kind.label()))
            );
        }
        assert_eq!(FAMILIES.len(), 12);
        assert!(!rendered.contains("oagw_upstream_connections"));
    }

    #[test]
    fn renders_a_histogram_series_over_twelve_buckets() {
        let registry = MetricsRegistry::new();
        registry.observe(
            "oagw_request_duration_seconds",
            &[
                (MetricLabelSet::HOST, String::from("a")),
                (MetricLabelSet::HTTP_ROUTE, String::from("/v1/t")),
                (MetricLabelSet::PHASE, String::from("total")),
            ],
            0.02,
        );
        let rendered = registry.render();
        for bound in HISTOGRAM_BUCKETS {
            assert!(rendered.contains(&format!("le=\"{}\"", render_bound(bound))));
        }
        assert!(rendered.contains("le=\"+Inf\""));
        assert!(rendered.contains("_sum"));
        assert!(rendered.contains("_count"));
        assert!(rendered.contains("oagw_request_duration_seconds_bucket"));
    }

    #[test]
    fn renders_a_histogram_series_per_phase() {
        let registry = MetricsRegistry::new();
        for phase in PHASES {
            registry.observe(
                "oagw_request_duration_seconds",
                &[
                    (MetricLabelSet::HOST, String::from("a")),
                    (MetricLabelSet::HTTP_ROUTE, String::from("/v1/t")),
                    (MetricLabelSet::PHASE, String::from(phase)),
                ],
                0.01,
            );
        }
        let rendered = registry.render();
        for phase in PHASES {
            assert!(rendered.contains(&format!("phase=\"{phase}\"")));
        }
    }

    #[test]
    fn reports_a_gauge_that_returns_to_its_prior_value() {
        let registry = MetricsRegistry::new();
        registry.add("oagw_requests_in_flight", &[(MetricLabelSet::HOST, String::from("a"))], 1.0);
        registry.add("oagw_requests_in_flight", &[(MetricLabelSet::HOST, String::from("a"))], -1.0);
        let rendered = registry.render();
        assert!(rendered.contains("oagw_requests_in_flight{host=\"a\"} 0"));
    }

    #[test]
    fn samples_a_high_volume_success_record_and_never_a_failed_one() {
        let sink = Arc::new(CollectingSink::new());
        let observability = Observability::with_sink(Arc::clone(&sink) as Arc<dyn AuditSink>);
        observability.emit(
            event("proxy_request.succeeded"),
            true,
            SamplingDecision::Drop,
        );
        assert!(sink.records().is_empty());
        observability.emit(
            event("proxy_request.succeeded"),
            true,
            SamplingDecision::Keep,
        );
        assert_eq!(sink.records().len(), 1);
        observability.emit(
            event("proxy_request.failed"),
            true,
            SamplingDecision::Drop,
        );
        assert_eq!(sink.records().len(), 2);
    }

    #[test]
    fn bounds_the_authentication_failure_records() {
        let sink = Arc::new(CollectingSink::new());
        let observability = Observability::with_sink(Arc::clone(&sink) as Arc<dyn AuditSink>);
        for _ in 0..(AUTH_FAILURE_LOG_LIMIT * 3) {
            observability.emit(event("auth.failed"), false, SamplingDecision::Keep);
        }
        assert_eq!(
            sink.records().len(),
            usize::try_from(AUTH_FAILURE_LOG_LIMIT).unwrap_or(0)
        );
    }

    #[test]
    fn reports_the_breaker_states_and_transitions() {
        let registry = MetricsRegistry::new();
        registry.set(
            "oagw_circuit_breaker_state",
            &[(MetricLabelSet::HOST, String::from("a"))],
            breaker_gauge(BreakerPhase::Open),
        );
        registry.increment(
            "oagw_circuit_breaker_transitions_total",
            &[
                (MetricLabelSet::HOST, String::from("a")),
                (MetricLabelSet::FROM_STATE, String::from("closed")),
                (MetricLabelSet::TO_STATE, String::from("open")),
            ],
        );
        let rendered = registry.render();
        assert!(rendered.contains("oagw_circuit_breaker_state{host=\"a\"} 1"));
        assert!(rendered.contains("oagw_circuit_breaker_transitions_total{host=\"a\",from_state=\"closed\",to_state=\"open\"} 1"));
    }

    #[test]
    fn reports_the_routing_families() {
        let registry = MetricsRegistry::new();
        registry.increment(
            "oagw_routing_endpoint_selected",
            &[
                (MetricLabelSet::UPSTREAM_ID, String::from("u")),
                (MetricLabelSet::ENDPOINT_HOST, String::from("h")),
                (MetricLabelSet::SELECTION_METHOD, String::from("round_robin")),
            ],
        );
        let rendered = registry.render();
        assert!(rendered.contains(
            "oagw_routing_endpoint_selected{upstream_id=\"u\",endpoint_host=\"h\",selection_method=\"round_robin\"} 1"
        ));
    }

    #[test]
    fn reads_the_correlation_header_by_its_lowercase_name() {
        let headers = vec![
            (String::from("Accept"), String::from("*/*")),
            (String::from("X-Request-Id"), String::from("caller-1")),
        ];
        assert_eq!(correlation_header(&headers), Some("caller-1"));
        assert_eq!(correlation_header(&[]), None);
    }

    #[test]
    fn formats_a_timestamp_as_rfc3339() {
        assert_eq!(
            timestamp_of(UNIX_EPOCH),
            String::from("1970-01-01T00:00:00Z")
        );
        let later = UNIX_EPOCH + Duration::from_secs(1_767_225_600);
        assert!(timestamp_of(later).ends_with("Z"));
        assert_eq!(timestamp_of(later).len(), 20);
    }

    #[test]
    fn stamps_the_four_phases_in_order() {
        let mut timings = PhaseTimings::started();
        timings.resolved();
        timings.chained();
        timings.forwarded();
        let durations = timings.durations();
        assert_eq!(durations.len(), 4);
        assert_eq!(durations[0].0, "resolve");
        assert_eq!(durations[1].0, "chain");
        assert_eq!(durations[2].0, "upstream");
        assert_eq!(durations[3].0, "total");
        assert!(durations[3].1 >= durations[2].1);
    }

    #[test]
    fn omits_a_phase_the_path_never_reached() {
        let mut timings = PhaseTimings::started();
        timings.resolved();
        let names: Vec<&str> = timings
            .durations()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, vec!["resolve", "total"]);
        assert!(timings.durations().into_iter().all(|(_, seconds)| seconds
            >= 0.0));
    }

    #[test]
    fn classifies_an_exchange_failure_and_its_error_type() {
        let mut exchange = Exchange {
            host: Some(String::from("api.example.com")),
            route: Some(String::from("/v1/things/{id}")),
            method: String::from("GET"),
            status: Some(200),
            ..Exchange::default()
        };
        assert!(!exchange.failed());
        assert!(exchange.error_type().is_none());
        assert!(!exchange.high_volume());
        exchange.status = Some(502);
        assert!(exchange.failed());
        assert_eq!(exchange.error_type(), Some(ERROR_TYPE_UPSTREAM));
        exchange.status = Some(404);
        exchange.error = Some(ErrorKind::RouteNotFound);
        assert_eq!(exchange.error_type(), Some("route.not_found"));
        exchange.route = Some(String::from("/v1/things"));
        assert!(exchange.high_volume());
    }

    #[test]
    fn observes_a_finished_exchange_once() {
        let sink = Arc::new(CollectingSink::new());
        let observability = Observability::with_sink(Arc::clone(&sink) as Arc<dyn AuditSink>);
        let mut correlation = CorrelationContext::assign(Some("req-obs"), None, None);
        correlation.sampling = SamplingDecision::Keep;
        let mut timings = PhaseTimings::started();
        timings.resolved();
        observability.raise_in_flight("api.example.com");
        let exchange = Exchange {
            host: Some(String::from("api.example.com")),
            route: Some(String::from("/v1/things")),
            method: String::from("GET"),
            status: Some(200),
            timings: Some(timings),
            request_size: 10,
            response_size: Some(20),
            upstream_resolved: true,
            ..Exchange::default()
        };
        observability.observe(&exchange, Some(&correlation));
        let rendered = observability.render();
        assert!(rendered.contains("oagw_requests_total{host=\"api.example.com\",http.request.method=\"GET\",http.route=\"/v1/things\",http.response.status_code=\"200\"} 1"));
        assert!(rendered.contains("oagw_requests_in_flight{host=\"api.example.com\"} 0"));
        assert!(!rendered.contains("oagw_errors_total{"));
        let records = sink.records();
        assert_eq!(records.len(), 1);
        assert!(records[0].contains("\"request_id\":\"req-obs\""));
        assert!(records[0].contains("\"host\":\"api.example.com\""));
        assert!(records[0].contains("\"event\":\"proxy_request.succeeded\""));
    }

    #[test]
    fn observes_a_failed_exchange_with_its_error_type() {
        let sink = Arc::new(CollectingSink::new());
        let observability = Observability::with_sink(Arc::clone(&sink) as Arc<dyn AuditSink>);
        let exchange = Exchange {
            host: Some(String::from("api.example.com")),
            route: Some(String::from("/v1/things")),
            method: String::from("GET"),
            status: Some(404),
            error: Some(ErrorKind::RouteNotFound),
            upstream_resolved: true,
            ..Exchange::default()
        };
        observability.observe(&exchange, None);
        let rendered = observability.render();
        assert!(rendered.contains(
            "oagw_errors_total{host=\"api.example.com\",http.route=\"/v1/things\",error_type=\"route.not_found\"} 1"
        ));
        let records = sink.records();
        assert_eq!(records.len(), 1);
        assert!(records[0].contains("\"event\":\"proxy_request.failed\""));
        assert!(records[0].contains("\"error_type\":\"route.not_found\""));
    }

    #[test]
    fn defers_an_observation_to_the_session_end() {
        let sink = Arc::new(CollectingSink::new());
        let observability =
            Arc::new(Observability::with_sink(Arc::clone(&sink) as Arc<dyn AuditSink>));
        let session = Arc::new(Mutex::new(
            crate::domain::stream::StreamSession::open_for_incremental(
                Uuid::nil(),
                Uuid::nil(),
                None,
            ),
        ));
        session.lock().moved = 42;
        observability.raise_in_flight("api.example.com");
        let exchange = Exchange {
            host: Some(String::from("api.example.com")),
            route: Some(String::from("/v1/stream/{id}")),
            method: String::from("GET"),
            status: Some(200),
            session: Some(Arc::clone(&session)),
            ..Exchange::default()
        };
        {
            let _deferred = Arc::clone(&observability).defer(exchange, None);
            assert!(sink.records().is_empty());
        }
        let records = sink.records();
        assert_eq!(records.len(), 1);
        assert!(records[0].contains("\"response_size\":\"42\""));
        assert!(
            observability
                .render()
                .contains("oagw_requests_in_flight{host=\"api.example.com\"} 0")
        );
    }
}
