//! Data-plane metrics of the OAGW gear (DESIGN section 4.2).
//!
//! The registry is **in-process**: every instrument is a [`DashMap`] keyed by
//! the metric name plus its sorted label set, and
//! [`MetricsRegistry::render`] emits the Prometheus text exposition format
//! (`# HELP`, `# TYPE`, one sample per series, label values escaped). Serving
//! the text format locally keeps the `/oagw/v1/metrics` endpoint dependency
//! free and scrapeable by any collector, which matters because the platform's
//! OpenTelemetry exporter is not configured in every environment.
//!
//! ## Documented deviation
//!
//! When a global OpenTelemetry meter provider *is* installed, the counters and
//! the duration histogram are additionally emitted through the
//! [`opentelemetry`] global API (`oagw` meter, same instrument names and
//! labels). The gauges and the remaining counters stay local-only: their
//! label sets (`from_state`, `state`, `endpoint`, `selection_method`) are
//! gear-internal dimensions the platform telemetry pipeline does not model.
//!
//! ## Cardinality
//!
//! No label value is ever taken from the client. The `http.route` label of
//! [`REQUESTS_TOTAL`], [`ERRORS_TOTAL`] and [`REQUEST_DURATION_SECONDS`] and
//! the `path` label of the two rate-limit instruments all carry the
//! [`route_label`] of the request: the *configured* route pattern (its
//! `match.http.path`, prefixed by its method) or the fixed [`UNMATCHED_ROUTE`]
//! literal when no route resolved. A thousand requests against one route are
//! therefore one series, and a path suffix such as `/api/orders/42/items`
//! cannot mint a series of its own — the raw request path is never recorded.
//! The remaining labels are bounded: `host` carries the *upstream alias*
//! (DESIGN §4.2), which the registry bounds for every request that matched one
//! — [`host_label`] folds it onto [`UNMATCHED_HOST`] when nothing resolved, so
//! an invented alias cannot mint a series either — and `status_class`,
//! `error_type`, `phase`, `selection_method` and `endpoint` are taken from
//! configuration or from a bounded enumeration. The one exception is the
//! in-flight gauge, which is incremented before the request resolves and so
//! cannot take its bound from the matched route; it admits at most
//! [`MAX_IN_FLIGHT_HOSTS`] distinct hosts and folds the rest onto
//! [`UNMATCHED_HOST`].

use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::Arc;

use dashmap::DashMap;
use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::{Counter, Histogram};
use parking_lot::Mutex;

/// `oagw_requests_total{host, http.request.method, http.route, http.response.status_code}`.
pub const REQUESTS_TOTAL: &str = "oagw_requests_total";
/// `oagw_errors_total{host, http.route, error_type}`.
pub const ERRORS_TOTAL: &str = "oagw_errors_total";
/// `oagw_rate_limit_exceeded_total{host, path}`.
pub const RATE_LIMIT_EXCEEDED_TOTAL: &str = "oagw_rate_limit_exceeded_total";
/// `oagw_circuit_breaker_transitions_total{host, from_state, to_state}`.
pub const CIRCUIT_BREAKER_TRANSITIONS_TOTAL: &str = "oagw_circuit_breaker_transitions_total";
/// `oagw_routing_target_host_used{upstream_id, endpoint_host}`.
pub const ROUTING_TARGET_HOST_USED: &str = "oagw_routing_target_host_used";
/// `oagw_routing_endpoint_selected{upstream_id, endpoint_host, selection_method}`.
pub const ROUTING_ENDPOINT_SELECTED: &str = "oagw_routing_endpoint_selected";
/// `oagw_requests_in_flight{host}`.
pub const REQUESTS_IN_FLIGHT: &str = "oagw_requests_in_flight";
/// `oagw_circuit_breaker_state{host}`.
pub const CIRCUIT_BREAKER_STATE: &str = "oagw_circuit_breaker_state";
/// `oagw_rate_limit_usage_ratio{host, path}`.
pub const RATE_LIMIT_USAGE_RATIO: &str = "oagw_rate_limit_usage_ratio";
/// `oagw_upstream_available{host, endpoint}`.
pub const UPSTREAM_AVAILABLE: &str = "oagw_upstream_available";
/// `oagw_upstream_connections{host, state}`.
pub const UPSTREAM_CONNECTIONS: &str = "oagw_upstream_connections";
/// `oagw_request_duration_seconds{host, http.route, phase}`.
pub const REQUEST_DURATION_SECONDS: &str = "oagw_request_duration_seconds";

/// Histogram buckets of [`REQUEST_DURATION_SECONDS`], in seconds (DESIGN 4.2).
pub const DURATION_BUCKETS: [f64; 12] = [
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// `http.route`/`path` label value when no route resolved for the request.
///
/// A fixed literal rather than the request path: the request path is
/// client-controlled and would mint one series per distinct suffix.
pub const UNMATCHED_ROUTE: &str = "unmatched";

/// `host` label value used when no upstream resolved for the request.
///
/// The `host` label normally carries the upstream alias (DESIGN §4.2), which
/// the registry bounds. When no upstream resolved the alias is still whatever
/// the client typed in the URL, so it would mint one series per invented name.
/// Such requests collapse onto this literal instead, exactly as their route
/// collapses onto [`UNMATCHED_ROUTE`].
pub const UNMATCHED_HOST: &str = "unmatched";

/// `phase` label value of the full proxy exchange.
pub const PHASE_TOTAL: &str = "total";
/// `phase` label value of the connection establishment step.
pub const PHASE_CONNECT: &str = "connect";
/// `phase` label value of the upstream header exchange.
pub const PHASE_UPSTREAM: &str = "upstream";

/// `http.response.status_code` label bucket for successful responses.
pub const STATUS_2XX: &str = "2xx";
/// `http.response.status_code` label bucket for redirect responses.
pub const STATUS_3XX: &str = "3xx";
/// `http.response.status_code` label bucket for client-error responses.
pub const STATUS_4XX: &str = "4xx";
/// `http.response.status_code` label bucket for server-error responses.
pub const STATUS_5XX: &str = "5xx";

/// Metric names rendered as gauges rather than counters.
const GAUGE_NAMES: [&str; 5] = [
    REQUESTS_IN_FLIGHT,
    CIRCUIT_BREAKER_STATE,
    RATE_LIMIT_USAGE_RATIO,
    UPSTREAM_AVAILABLE,
    UPSTREAM_CONNECTIONS,
];

/// One series: a metric name plus its label set, in the declaration order of
/// the call site so the rendering matches the documented label list exactly
/// while identical series still collapse into one entry.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct SeriesKey {
    name: &'static str,
    labels: Vec<(String, String)>,
}

impl SeriesKey {
    fn new(name: &'static str, labels: &[(&str, String)]) -> Self {
        let labels: Vec<(String, String)> = labels
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect();
        Self { name, labels }
    }

    fn render_labels(&self, extra: Option<(&str, &str)>) -> String {
        if self.labels.is_empty() && extra.is_none() {
            return String::new();
        }
        let mut rendered = String::new();
        let mut first = true;
        for (key, value) in &self.labels {
            let separator = if first { "" } else { "," };
            first = false;
            let _ = write!(rendered, "{separator}{}=\"{}\"", escape(key), escape(value));
        }
        if let Some((key, value)) = extra {
            let separator = if first { "" } else { "," };
            let _ = write!(rendered, "{separator}{}=\"{}\"", escape(key), escape(value));
        }
        format!("{{{rendered}}}")
    }
}

/// Escapes a Prometheus label name or value.
fn escape(value: &str) -> String {
    let mut rendered = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => rendered.push_str("\\\\"),
            '"' => rendered.push_str("\\\""),
            '\n' => rendered.push_str("\\n"),
            other => rendered.push(other),
        }
    }
    rendered
}

/// Renders a floating-point sample the way Prometheus expects (`1` not `1.0`
/// for integral values is also accepted, but `1.0` is unambiguous).
fn render_number(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_owned();
    }
    if value.is_infinite() {
        return if value.is_sign_negative() {
            "-Inf".to_owned()
        } else {
            "+Inf".to_owned()
        };
    }
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// Accumulated histogram state of one series.
#[derive(Debug, Default, Clone)]
struct HistogramState {
    counts: Vec<u64>,
    sum: f64,
    count: u64,
}

impl HistogramState {
    fn observe(&mut self, buckets: usize, value: f64) {
        if self.counts.len() != buckets {
            self.counts = vec![0; buckets];
        }
        let index = DURATION_BUCKETS
            .iter()
            .take(buckets)
            .position(|bound| value <= *bound)
            .unwrap_or(buckets.saturating_sub(1));
        if let Some(slot) = self.counts.get_mut(index) {
            *slot += 1;
        }
        self.sum += value;
        self.count += 1;
    }
}

/// OpenTelemetry instruments mirrored from the local registry.
#[derive(Debug, Clone)]
struct OtelInstruments {
    requests: Counter<u64>,
    errors: Counter<u64>,
    duration: Histogram<f64>,
}

/// In-process metrics registry of the data plane.
///
/// Cheap to clone through an [`Arc`]; every instrument is a concurrent map, so
/// recording never blocks a proxy request on another one.
#[derive(Debug, Default)]
pub struct MetricsRegistry {
    scalars: DashMap<SeriesKey, f64>,
    histograms: DashMap<SeriesKey, HistogramState>,
    otel: std::sync::OnceLock<OtelInstruments>,
    /// Hosts the in-flight gauge has admitted, bounded at
    /// [`MAX_IN_FLIGHT_HOSTS`].
    ///
    /// The gauge is incremented *before* the request resolves (that is the
    /// point of measuring concurrency), so unlike the other instruments it
    /// cannot take its bound from the matched route. Admitting a host once and
    /// keeping it — never removing it on decrement, which would let an
    /// attacker alternate between two names to stay under the cap while
    /// minting series — keeps the set monotonic and therefore bounded, and
    /// guarantees that an increment and its decrement always resolve to the
    /// same series.
    in_flight_hosts: Mutex<HashSet<String>>,
}

/// Maximum distinct `host` series the in-flight gauge tracks before folding
/// further names onto [`UNMATCHED_HOST`].
///
/// Far above any real fleet of configured upstreams; the cap exists so a
/// caller cannot mint unbounded zero-valued series by inventing alias names.
pub const MAX_IN_FLIGHT_HOSTS: usize = 4096;

impl MetricsRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Bumps `scalars[key]` by `delta`.
    fn bump(&self, key: SeriesKey, delta: f64) {
        *self.scalars.entry(key).or_insert(0.0) += delta;
    }

    /// Overwrites `scalars[key]`.
    fn set(&self, key: SeriesKey, value: f64) {
        self.scalars.insert(key, value);
    }

    /// Records one completed proxy request.
    pub fn record_request(&self, host: &str, method: &str, route: &str, status: u16) {
        let labels = [
            ("host", host.to_owned()),
            ("http.request.method", normalize_method(method)),
            ("http.route", route.to_owned()),
            ("http.response.status_code", status_class(status).to_owned()),
        ];
        self.bump(SeriesKey::new(REQUESTS_TOTAL, &labels), 1.0);
        self.otel
            .get_or_init(build_otel_instruments)
            .requests
            .add(1, &attributes(&labels));
    }

    /// Records one gateway-side error.
    pub fn record_error(&self, host: &str, route: &str, error_type: &str) {
        let labels = [
            ("host", host.to_owned()),
            ("http.route", route.to_owned()),
            ("error_type", error_type.to_owned()),
        ];
        self.bump(SeriesKey::new(ERRORS_TOTAL, &labels), 1.0);
        self.otel
            .get_or_init(build_otel_instruments)
            .errors
            .add(1, &attributes(&labels));
    }

    /// Records one rate-limit rejection.
    pub fn record_rate_limit_exceeded(&self, host: &str, route: &str) {
        let labels = [("host", host.to_owned()), ("path", route.to_owned())];
        self.bump(SeriesKey::new(RATE_LIMIT_EXCEEDED_TOTAL, &labels), 1.0);
    }

    /// Records one circuit-breaker state transition.
    pub fn record_circuit_breaker_transition(&self, host: &str, from: &str, to: &str) {
        let labels = [
            ("host", host.to_owned()),
            ("from_state", from.to_owned()),
            ("to_state", to.to_owned()),
        ];
        self.bump(
            SeriesKey::new(CIRCUIT_BREAKER_TRANSITIONS_TOTAL, &labels),
            1.0,
        );
    }

    /// Records that a routing decision used `endpoint_host` of `upstream_id`.
    pub fn record_target_host_used(&self, upstream_id: &str, endpoint_host: &str) {
        let labels = [
            ("upstream_id", upstream_id.to_owned()),
            ("endpoint_host", endpoint_host.to_owned()),
        ];
        self.bump(SeriesKey::new(ROUTING_TARGET_HOST_USED, &labels), 1.0);
    }

    /// Records that `endpoint_host` was selected with `selection_method`.
    pub fn record_endpoint_selected(
        &self,
        upstream_id: &str,
        endpoint_host: &str,
        selection_method: &str,
    ) {
        let labels = [
            ("upstream_id", upstream_id.to_owned()),
            ("endpoint_host", endpoint_host.to_owned()),
            ("selection_method", selection_method.to_owned()),
        ];
        self.bump(SeriesKey::new(ROUTING_ENDPOINT_SELECTED, &labels), 1.0);
    }

    /// Increments the in-flight gauge of `host`.
    pub fn inc_in_flight(&self, host: &str) {
        let key = self.in_flight_key(host);
        self.bump(key, 1.0);
    }

    /// Decrements the in-flight gauge of `host`.
    pub fn dec_in_flight(&self, host: &str) {
        let key = self.in_flight_key(host);
        self.bump(key, -1.0);
    }

    /// The series an in-flight increment and its decrement both resolve to.
    ///
    /// Admission is monotonic — a host is never removed — so both sides of an
    /// exchange always land on the same series even once the cap is reached.
    ///
    /// [`UNMATCHED_HOST`] needs no admission: it is the fold target of every
    /// request that resolved to nothing, and no upstream can claim it, because
    /// the alias validator reserves the word
    /// ([`crate::domain::validation::is_valid_alias`]).
    fn in_flight_key(&self, host: &str) -> SeriesKey {
        let label = {
            let mut hosts = self.in_flight_hosts.lock();
            if hosts.contains(host) || hosts.len() < MAX_IN_FLIGHT_HOSTS {
                hosts.insert(host.to_owned());
                host
            } else {
                UNMATCHED_HOST
            }
        };
        SeriesKey::new(REQUESTS_IN_FLIGHT, &[("host", label.to_owned())])
    }

    /// Publishes the circuit-breaker state gauge of `host`.
    pub fn set_circuit_breaker_state(&self, host: &str, state: u8) {
        let key = SeriesKey::new(CIRCUIT_BREAKER_STATE, &[("host", host.to_owned())]);
        self.set(key, f64::from(state));
    }

    /// Publishes the consumed fraction of the rate-limit budget of `host`/`route`.
    pub fn set_rate_limit_usage_ratio(&self, host: &str, route: &str, ratio: f64) {
        let labels = [("host", host.to_owned()), ("path", route.to_owned())];
        let clamped = ratio.clamp(0.0, 1.0);
        self.set(SeriesKey::new(RATE_LIMIT_USAGE_RATIO, &labels), clamped);
    }

    /// Publishes the availability of one upstream endpoint (`1`/`0`).
    pub fn set_upstream_available(&self, host: &str, endpoint: &str, available: bool) {
        let labels = [("host", host.to_owned()), ("endpoint", endpoint.to_owned())];
        self.set(
            SeriesKey::new(UPSTREAM_AVAILABLE, &labels),
            if available { 1.0 } else { 0.0 },
        );
    }

    /// Publishes the pooled connection count of `host` in `state`.
    pub fn set_upstream_connections(&self, host: &str, state: &str, count: u64) {
        let labels = [("host", host.to_owned()), ("state", state.to_owned())];
        self.set(SeriesKey::new(UPSTREAM_CONNECTIONS, &labels), count as f64);
    }

    /// Observes one duration in seconds.
    pub fn record_duration(&self, host: &str, route: &str, phase: &str, seconds: f64) {
        let labels = [
            ("host", host.to_owned()),
            ("http.route", route.to_owned()),
            ("phase", phase.to_owned()),
        ];
        self.histograms
            .entry(SeriesKey::new(REQUEST_DURATION_SECONDS, &labels))
            .and_modify(|state| state.observe(DURATION_BUCKETS.len(), seconds))
            .or_insert_with(|| {
                let mut state = HistogramState::default();
                state.observe(DURATION_BUCKETS.len(), seconds);
                state
            });
        self.otel
            .get_or_init(build_otel_instruments)
            .duration
            .record(seconds, &attributes(&labels));
    }

    /// Renders the registry in the Prometheus text exposition format.
    #[must_use]
    pub fn render(&self) -> String {
        let mut scalars: Vec<(SeriesKey, f64)> = self
            .scalars
            .iter()
            .map(|entry| (entry.key().clone(), *entry.value()))
            .collect();
        scalars.sort_by(|left, right| left.0.cmp(&right.0));
        let mut histograms: Vec<(SeriesKey, HistogramState)> = self
            .histograms
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect();
        histograms.sort_by(|left, right| left.0.cmp(&right.0));

        let mut rendered = String::new();
        let mut emitted: Vec<&'static str> = Vec::new();
        for (key, value) in scalars {
            if !emitted.contains(&key.name) {
                emitted.push(key.name);
                let kind = if GAUGE_NAMES.contains(&key.name) {
                    "gauge"
                } else {
                    "counter"
                };
                write_header(&mut rendered, key.name, kind);
            }
            let _ = writeln!(
                rendered,
                "{}{} {}",
                key.name,
                key.render_labels(None),
                render_number(value)
            );
        }
        for (key, state) in histograms {
            if !emitted.contains(&key.name) {
                emitted.push(key.name);
                write_header(&mut rendered, key.name, "histogram");
            }
            for (index, bound) in DURATION_BUCKETS.iter().enumerate() {
                // Prometheus histograms are cumulative: `le` counts every
                // observation up to and including this boundary.
                let cumulative: u64 = state.counts.iter().take(index + 1).copied().sum();
                let _ = writeln!(
                    rendered,
                    "{}_bucket{} {}",
                    key.name,
                    key.render_labels(Some(("le", &format!("{bound}")))),
                    cumulative
                );
            }
            let _ = writeln!(
                rendered,
                "{}_bucket{} {}",
                key.name,
                key.render_labels(Some(("le", "+Inf"))),
                state.count
            );
            let _ = writeln!(
                rendered,
                "{}_sum{} {}",
                key.name,
                key.render_labels(None),
                render_number(state.sum)
            );
            let _ = writeln!(
                rendered,
                "{}_count{} {}",
                key.name,
                key.render_labels(None),
                state.count
            );
        }
        rendered
    }
}

/// Converts a label set into OpenTelemetry attributes.
fn attributes(labels: &[(&str, String)]) -> Vec<KeyValue> {
    labels
        .iter()
        .map(|(key, value)| KeyValue::new(key.to_string(), value.clone()))
        .collect()
}

/// Builds the OpenTelemetry instruments mirrored from the local registry.
fn build_otel_instruments() -> OtelInstruments {
    let meter = global::meter("oagw");
    OtelInstruments {
        requests: meter.u64_counter(REQUESTS_TOTAL).build(),
        errors: meter.u64_counter(ERRORS_TOTAL).build(),
        duration: meter
            .f64_histogram(REQUEST_DURATION_SECONDS)
            .with_boundaries(DURATION_BUCKETS.to_vec())
            .build(),
    }
}

/// Writes the `# HELP` / `# TYPE` pair of one metric.
fn write_header(out: &mut String, name: &'static str, kind: &'static str) {
    let _ = writeln!(out, "# HELP {name} {}", help_of(name));
    let _ = writeln!(out, "# TYPE {name} {kind}");
}

/// The `# HELP` text of one metric.
fn help_of(name: &'static str) -> &'static str {
    match name {
        REQUESTS_TOTAL => "Proxy requests answered by this gateway.",
        ERRORS_TOTAL => "Errors produced by the gateway itself.",
        RATE_LIMIT_EXCEEDED_TOTAL => "Requests rejected because the rate budget was exhausted.",
        CIRCUIT_BREAKER_TRANSITIONS_TOTAL => "Circuit-breaker state transitions.",
        ROUTING_TARGET_HOST_USED => "Times an endpoint host was used as the proxy target.",
        ROUTING_ENDPOINT_SELECTED => "Endpoint selections, by selection method.",
        REQUESTS_IN_FLIGHT => "Proxy requests currently in flight.",
        CIRCUIT_BREAKER_STATE => "Circuit-breaker state (0 closed, 1 half-open, 2 open).",
        RATE_LIMIT_USAGE_RATIO => "Consumed fraction of the rate-limit budget.",
        UPSTREAM_AVAILABLE => "Whether an upstream endpoint is considered available.",
        UPSTREAM_CONNECTIONS => "Pooled upstream connections by state.",
        REQUEST_DURATION_SECONDS => "Proxy exchange duration.",
        _ => "OAGW metric.",
    }
}

/// The `http.request.method` label value of `method`, falling back to `_OTHER`
/// so an arbitrary verb cannot create unbounded label cardinality.
#[must_use]
pub fn normalize_method(method: &str) -> String {
    const KNOWN: [&str; 9] = [
        "GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS", "TRACE", "CONNECT",
    ];
    let upper = method.to_ascii_uppercase();
    if KNOWN.contains(&upper.as_str()) {
        upper
    } else {
        "_OTHER".to_owned()
    }
}

/// The `http.response.status_code` class label of `status`.
#[must_use]
pub const fn status_class(status: u16) -> &'static str {
    match status / 100 {
        2 => STATUS_2XX,
        3 => STATUS_3XX,
        4 => STATUS_4XX,
        _ => STATUS_5XX,
    }
}

/// The route label of one request: the *configured* route pattern, never the
/// raw request path.
///
/// `pattern` is the route's `match.http.path` as declared (`/v1/orders`). It is
/// prefixed with the normalised HTTP method, because two routes may share a
/// path with disjoint method sets, and the label has to stay injective over the
/// route table for the `path` label of the rate-limit instruments to be
/// readable. `None` (no route matched, or a route without an HTTP match)
/// yields [`UNMATCHED_ROUTE`].
///
/// ```
/// # use oagw::domain::metrics::route_label;
/// assert_eq!(route_label("GET", Some("/v1/orders")), "GET /v1/orders");
/// assert_eq!(route_label("GET", None), "unmatched");
/// ```
#[must_use]
pub fn route_label(method: &str, pattern: Option<&str>) -> String {
    match pattern {
        Some(pattern) => format!("{} {}", normalize_method(method), pattern),
        None => UNMATCHED_ROUTE.to_owned(),
    }
}

/// The `host` label of a request: the resolved upstream alias, or the fixed
/// [`UNMATCHED_HOST`] literal when no route resolved.
///
/// Paired with [`route_label`] so an unmatched request is bounded on *both* of
/// its client-controlled dimensions — see the module's `Cardinality` section.
#[must_use]
pub fn host_label<'a>(route: &str, alias: &'a str) -> &'a str {
    if route == UNMATCHED_ROUTE {
        UNMATCHED_HOST
    } else {
        alias
    }
}

/// Clones the registry behind an [`Arc`], the shape handlers receive.
#[must_use]
pub fn shared() -> Arc<MetricsRegistry> {
    Arc::new(MetricsRegistry::new())
}

#[cfg(test)]
#[path = "metrics_tests.rs"]
mod tests;
