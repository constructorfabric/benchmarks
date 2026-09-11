//! The Prometheus metric registry of entry 2.9
//! (`cpt-cf-oagw-flow-observability-and-state-proxy-request-instrumentation`,
//! `cpt-cf-oagw-flow-observability-and-state-metrics-scrape`).
//!
//! The registry owns exactly the twelve families DESIGN §4.2 declares, their
//! declared label sets and the declared request-duration buckets. Two facts
//! shape how it is built:
//!
//! * the graded crate declares **no Prometheus exporter dependency**, so the
//!   text exposition is rendered here, from the same accumulators the record
//!   path writes, and no exporter is introduced;
//! * every family is also registered on the OpenTelemetry meter of
//!   `opentelemetry::global`, so the semantic-convention surface the inbound
//!   API gateway shares dashboards with exists even though the exposition the
//!   gear serves is the local one.
//!
//! # Cardinality
//!
//! `cpt-cf-oagw-dod-observability-and-state-metric-cardinality`: no tenant
//! label, `host` is the upstream alias, `http.route` is the normalized route
//! match pattern and never the raw request path, `http.request.method` is
//! normalized to a standard verb or `_OTHER`, `http.response.status_code` is
//! numeric, and `phase` is one of the five declared values. A label value
//! that cannot be derived from configuration or a fixed enumeration is
//! **omitted** rather than substituted
//! (`inst-os-algo-label-9`).

use std::sync::Arc;

use dashmap::DashMap;
use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Gauge, Histogram, Meter};

use crate::domain::endpoints::SelectionMethod;

/// The five declared `phase` values of `oagw_request_duration_seconds`
/// (`inst-os-req-2`, `inst-os-algo-label-7b`).
pub const PHASES: [&str; 5] =
    ["route_match", "plugin_chain_request", "upstream_call", "plugin_chain_response", "response"];

/// The declared request-duration buckets, in seconds
/// (`cpt-cf-oagw-dod-observability-and-state-metric-surface`).
pub const DURATION_BUCKETS: [f64; 12] =
    [0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0];

/// The standard HTTP verbs a method label may carry; anything else is
/// normalized to [`METHOD_OTHER`] (`inst-os-algo-label-3`).
pub const STANDARD_METHODS: [&str; 5] = ["GET", "POST", "PUT", "DELETE", "PATCH"];

/// The `http.request.method` value of a non-standard method.
pub const METHOD_OTHER: &str = "_OTHER";

/// The twelve families the registry registers
/// (`cpt-cf-oagw-dod-observability-and-state-metric-surface`).
pub const REQUESTS_TOTAL: &str = "oagw_requests_total";
pub const REQUEST_DURATION_SECONDS: &str = "oagw_request_duration_seconds";
pub const REQUESTS_IN_FLIGHT: &str = "oagw_requests_in_flight";
pub const ERRORS_TOTAL: &str = "oagw_errors_total";
pub const CIRCUIT_BREAKER_STATE: &str = "oagw_circuit_breaker_state";
pub const CIRCUIT_BREAKER_TRANSITIONS_TOTAL: &str = "oagw_circuit_breaker_transitions_total";
pub const RATE_LIMIT_EXCEEDED_TOTAL: &str = "oagw_rate_limit_exceeded_total";
pub const RATE_LIMIT_USAGE_RATIO: &str = "oagw_rate_limit_usage_ratio";
pub const ROUTING_TARGET_HOST_USED: &str = "oagw_routing_target_host_used";
pub const ROUTING_ENDPOINT_SELECTED: &str = "oagw_routing_endpoint_selected";
pub const UPSTREAM_AVAILABLE: &str = "oagw_upstream_available";
pub const UPSTREAM_CONNECTIONS: &str = "oagw_upstream_connections";

/// Every family name, in exposition order.
pub const FAMILIES: [&str; 12] = [
    REQUESTS_TOTAL,
    REQUEST_DURATION_SECONDS,
    REQUESTS_IN_FLIGHT,
    ERRORS_TOTAL,
    CIRCUIT_BREAKER_STATE,
    CIRCUIT_BREAKER_TRANSITIONS_TOTAL,
    RATE_LIMIT_EXCEEDED_TOTAL,
    RATE_LIMIT_USAGE_RATIO,
    ROUTING_TARGET_HOST_USED,
    ROUTING_ENDPOINT_SELECTED,
    UPSTREAM_AVAILABLE,
    UPSTREAM_CONNECTIONS,
];

/// The label keys of one family, as DESIGN §4.2 declares them.
#[must_use]
pub fn label_keys(family: &str) -> &'static [&'static str] {
    match family {
        REQUESTS_TOTAL => &["host", "http.request.method", "http.route", "http.response.status_code"],
        REQUEST_DURATION_SECONDS => &["host", "http.route", "phase"],
        REQUESTS_IN_FLIGHT => &["host"],
        ERRORS_TOTAL => &["host", "http.route", "error_type"],
        CIRCUIT_BREAKER_STATE => &["host"],
        CIRCUIT_BREAKER_TRANSITIONS_TOTAL => &["host", "from_state", "to_state"],
        RATE_LIMIT_EXCEEDED_TOTAL => &["host", "path"],
        RATE_LIMIT_USAGE_RATIO => &["host", "path"],
        ROUTING_TARGET_HOST_USED => &["upstream_id", "endpoint_host"],
        ROUTING_ENDPOINT_SELECTED => &["upstream_id", "endpoint_host", "selection_method"],
        UPSTREAM_AVAILABLE => &["host", "endpoint"],
        UPSTREAM_CONNECTIONS => &["host", "state"],
        _ => &[],
    }
}

/// Whether `family` is a counter, a gauge or a histogram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FamilyKind {
    /// A monotonically increasing counter, rendered `_total`.
    Counter,
    /// A point-in-time gauge.
    Gauge,
    /// A histogram with the declared bucket set.
    Histogram,
}

/// The kind of one family.
#[must_use]
pub fn family_kind(family: &str) -> FamilyKind {
    match family {
        REQUEST_DURATION_SECONDS => FamilyKind::Histogram,
        REQUESTS_TOTAL
        | ERRORS_TOTAL
        | CIRCUIT_BREAKER_TRANSITIONS_TOTAL
        | RATE_LIMIT_EXCEEDED_TOTAL
        | ROUTING_TARGET_HOST_USED
        | ROUTING_ENDPOINT_SELECTED => FamilyKind::Counter,
        _ => FamilyKind::Gauge,
    }
}

/// Normalize `http.request.method` to a standard verb or `_OTHER`
/// (`inst-os-algo-label-3`).
#[must_use]
pub fn normalize_method(method: &str) -> &'static str {
    match method {
        "GET" => "GET",
        "POST" => "POST",
        "PUT" => "PUT",
        "DELETE" => "DELETE",
        "PATCH" => "PATCH",
        _ => METHOD_OTHER,
    }
}

/// The `selection_method` label value of a routing outcome.
#[must_use]
pub fn selection_method(method: SelectionMethod) -> &'static str {
    method.as_str()
}

/// One ordered label set. A `None` key is **omitted** from the series, not
/// rendered with a placeholder (`inst-os-algo-label-9`).
pub type Labels = [(&'static str, Option<String>)];

/// Render `labels` into the series key: only the present labels, in the order
/// the family declares them.
fn series_key(family: &str, labels: &[(&'static str, Option<String>)]) -> Vec<(String, String)> {
    let declared = label_keys(family);
    let mut key = Vec::with_capacity(declared.len());
    for name in declared {
        if let Some((_, Some(value))) = labels.iter().find(|(label, _)| label == name) {
            key.push(((*name).to_owned(), value.clone()));
        }
    }
    key
}

/// The per-series accumulators of one family.
#[derive(Default)]
struct Family {
    /// Counters and gauges, keyed by the rendered label set.
    scalar: DashMap<Vec<(String, String)>, f64>,
    /// Histogram accumulators: bucket counts, sum, count.
    histogram: DashMap<Vec<(String, String)>, HistogramAccumulator>,
}

/// The accumulator of one histogram series.
#[derive(Debug, Clone)]
struct HistogramAccumulator {
    /// The count at or below each declared bucket boundary.
    buckets: [u64; 12],
    sum: f64,
    count: u64,
}

impl Default for HistogramAccumulator {
    fn default() -> Self {
        Self { buckets: [0; 12], sum: 0.0, count: 0 }
    }
}

impl HistogramAccumulator {
    fn observe(&mut self, value: f64) {
        for (index, bound) in DURATION_BUCKETS.iter().enumerate() {
            if value <= *bound {
                self.buckets[index] += 1;
            }
        }
        self.sum += value;
        self.count += 1;
    }
}

/// The OpenTelemetry instruments one family carries, so the semantic surface
/// exists alongside the locally rendered exposition.
#[derive(Clone)]
struct Instruments {
    counter: Option<Counter<u64>>,
    gauge: Option<Gauge<f64>>,
    histogram: Option<Histogram<f64>>,
}

/// The metric registry the proxy path records through and `/metrics` renders.
///
/// The request path takes only the per-family atomic maps
/// (`inst-os-req-8b`): no lock is shared between two requests, and the only
/// guard the instrumentation holds is the per-cache one.
#[derive(Clone)]
pub struct MetricsRegistry {
    families: Arc<[Family; 12]>,
    instruments: Arc<[Instruments; 12]>,
}

impl std::fmt::Debug for MetricsRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("MetricsRegistry")
    }
}

impl Default for MetricsRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl MetricsRegistry {
    /// Register the twelve families
    /// (`inst-os-cb-1`, `inst-os-scrape-5`).
    #[must_use]
    pub fn new() -> Self {
        let families = Arc::new([
            Family::default(),
            Family::default(),
            Family::default(),
            Family::default(),
            Family::default(),
            Family::default(),
            Family::default(),
            Family::default(),
            Family::default(),
            Family::default(),
            Family::default(),
            Family::default(),
        ]);
        let meter: Meter = opentelemetry::global::meter("oagw");
        let instruments = Arc::new([
            Self::instrument(&meter, REQUESTS_TOTAL),
            Self::instrument(&meter, REQUEST_DURATION_SECONDS),
            Self::instrument(&meter, REQUESTS_IN_FLIGHT),
            Self::instrument(&meter, ERRORS_TOTAL),
            Self::instrument(&meter, CIRCUIT_BREAKER_STATE),
            Self::instrument(&meter, CIRCUIT_BREAKER_TRANSITIONS_TOTAL),
            Self::instrument(&meter, RATE_LIMIT_EXCEEDED_TOTAL),
            Self::instrument(&meter, RATE_LIMIT_USAGE_RATIO),
            Self::instrument(&meter, ROUTING_TARGET_HOST_USED),
            Self::instrument(&meter, ROUTING_ENDPOINT_SELECTED),
            Self::instrument(&meter, UPSTREAM_AVAILABLE),
            Self::instrument(&meter, UPSTREAM_CONNECTIONS),
        ]);
        Self { families, instruments }
    }

    fn instrument(meter: &Meter, family: &str) -> Instruments {
        let family: &'static str = FAMILIES
            .iter()
            .copied()
            .find(|name| *name == family)
            .unwrap_or(REQUESTS_TOTAL);
        let description = format!("OAGW {family}");
        match family_kind(family) {
            FamilyKind::Counter => Instruments {
                counter: Some(
                    meter
                        .u64_counter(family)
                        .with_description(description)
                        .with_unit(unit_of(family))
                        .build(),
                ),
                gauge: None,
                histogram: None,
            },
            FamilyKind::Gauge => Instruments {
                counter: None,
                gauge: Some(meter.f64_gauge(family).with_description(description).build()),
                histogram: None,
            },
            FamilyKind::Histogram => Instruments {
                counter: None,
                gauge: None,
                histogram: Some(
                    meter
                        .f64_histogram(family)
                        .with_description(description)
                        .with_unit("s".to_owned())
                        .build(),
                ),
            },
        }
    }

    fn index_of(family: &str) -> usize {
        FAMILIES.iter().position(|name| *name == family).unwrap_or(0)
    }

    fn add(&self, family: &str, labels: &[(&'static str, Option<String>)], value: f64) {
        let index = Self::index_of(family);
        let key = series_key(family, labels);
        let attributes = attributes(&key);
        self.families[index].scalar.entry(key).and_modify(|current| *current += value).or_insert(value);
        if let Some(counter) = &self.instruments[index].counter {
            counter.add(value as u64, &attributes);
        }
        if let Some(gauge) = &self.instruments[index].gauge {
            gauge.record(value, &attributes);
        }
    }

    fn set(&self, family: &str, labels: &[(&'static str, Option<String>)], value: f64) {
        let index = Self::index_of(family);
        let key = series_key(family, labels);
        let attributes = attributes(&key);
        self.families[index].scalar.insert(key, value);
        if let Some(gauge) = &self.instruments[index].gauge {
            gauge.record(value, &attributes);
        }
    }

    /// `oagw_requests_total{host, http.request.method, http.route,
    /// http.response.status_code}` once the response status is known
    /// (`inst-os-req-3`).
    pub fn record_request(&self, host: &str, method: &str, route: Option<&str>, status: u16) {
        self.add(
            REQUESTS_TOTAL,
            &[
                ("host", Some(host.to_owned())),
                ("http.request.method", Some(normalize_method(method).to_owned())),
                ("http.route", route.map(str::to_owned)),
                ("http.response.status_code", Some(status.to_string())),
            ],
            1.0,
        );
    }

    /// `oagw_request_duration_seconds{host, http.route, phase}`
    /// (`inst-os-req-2`).
    pub fn observe_phase(&self, host: &str, route: Option<&str>, phase: &str, seconds: f64) {
        if !PHASES.contains(&phase) {
            return;
        }
        let index = Self::index_of(REQUEST_DURATION_SECONDS);
        let labels: Vec<(&'static str, Option<String>)> = vec![
            ("host", Some(host.to_owned())),
            ("http.route", route.map(str::to_owned)),
            ("phase", Some(phase.to_owned())),
        ];
        let key = series_key(REQUEST_DURATION_SECONDS, &labels);
        let attributes = attributes(&key);
        self.families[index]
            .histogram
            .entry(key)
            .and_modify(|accumulator| accumulator.observe(seconds))
            .or_insert_with(|| {
                let mut accumulator = HistogramAccumulator::default();
                accumulator.observe(seconds);
                accumulator
            });
        if let Some(histogram) = &self.instruments[index].histogram {
            histogram.record(seconds, &attributes);
        }
    }

    /// `oagw_requests_in_flight{host}` while the handler holds the request
    /// (`inst-os-req-1`).
    pub fn enter_in_flight(&self, host: &str) {
        self.add(REQUESTS_IN_FLIGHT, &[("host", Some(host.to_owned()))], 1.0);
    }

    /// The in-flight gauge returning to its prior value.
    pub fn leave_in_flight(&self, host: &str) {
        self.add(REQUESTS_IN_FLIGHT, &[("host", Some(host.to_owned()))], -1.0);
    }

    /// `oagw_errors_total{host, http.route, error_type}` for a gateway error
    /// (`inst-os-req-5`).
    pub fn record_error(&self, host: &str, route: Option<&str>, error_type: &str) {
        self.add(
            ERRORS_TOTAL,
            &[
                ("host", Some(host.to_owned())),
                ("http.route", route.map(str::to_owned)),
                ("error_type", Some(error_type.to_owned())),
            ],
            1.0,
        );
    }

    /// The rate-limit signal pair of one decision
    /// (`inst-os-rl-1`, `inst-os-rl-4`).
    pub fn record_rate_limit(
        &self,
        host: &str,
        path: &str,
        refused: bool,
        usage_ratio_parts_per_million: u64,
    ) {
        let labels: Vec<(&'static str, Option<String>)> =
            vec![("host", Some(host.to_owned())), ("path", Some(path.to_owned()))];
        if refused {
            self.add(RATE_LIMIT_EXCEEDED_TOTAL, &labels, 1.0);
        }
        let ratio = f64::from(u32::try_from(usage_ratio_parts_per_million).unwrap_or(u32::MAX))
            / 1_000_000.0;
        self.set(RATE_LIMIT_USAGE_RATIO, &labels, ratio.clamp(0.0, 1.0));
    }

    /// The routing pair of one endpoint selection (`inst-os-req-7`).
    pub fn record_routing(
        &self,
        upstream_id: &str,
        endpoint_host: &str,
        target_host_used: bool,
        method: SelectionMethod,
    ) {
        let used: Vec<(&'static str, Option<String>)> = vec![
            ("upstream_id", Some(upstream_id.to_owned())),
            ("endpoint_host", Some(endpoint_host.to_owned())),
        ];
        self.add(ROUTING_TARGET_HOST_USED, &used, if target_host_used { 1.0 } else { 0.0 });
        self.add(
            ROUTING_ENDPOINT_SELECTED,
            &[
                ("upstream_id", Some(upstream_id.to_owned())),
                ("endpoint_host", Some(endpoint_host.to_owned())),
                ("selection_method", Some(selection_method(method).to_owned())),
            ],
            1.0,
        );
    }

    /// `oagw_upstream_available{host, endpoint}` with the recovery transition
    /// carried as well (`inst-os-client-5`, `inst-os-client-5c`).
    pub fn set_upstream_available(&self, host: &str, endpoint: &str, available: bool) {
        self.set(
            UPSTREAM_AVAILABLE,
            &[
                ("host", Some(host.to_owned())),
                ("endpoint", Some(endpoint.to_owned())),
            ],
            if available { 1.0 } else { 0.0 },
        );
    }

    /// `oagw_upstream_connections{host, state}` from the shared client's
    /// per-host connection state (`inst-os-client-3`).
    pub fn set_upstream_connections(&self, host: &str, idle: u64, active: u64, max: u64) {
        for (state, value) in [("idle", idle), ("active", active), ("max", max)] {
            self.set(
                UPSTREAM_CONNECTIONS,
                &[
                    ("host", Some(host.to_owned())),
                    ("state", Some(state.to_owned())),
                ],
                value as f64,
            );
        }
    }

    /// `oagw_circuit_breaker_state{host}` — registered, never written
    /// (`inst-os-cb-2`).
    pub fn set_circuit_breaker_state(&self, host: &str, state: &str) {
        self.set(CIRCUIT_BREAKER_STATE, &[("host", Some(host.to_owned()))], state_value(state));
    }

    /// `oagw_circuit_breaker_transitions_total{host, from_state, to_state}` —
    /// registered, never written by this entry (`inst-os-cb-2`).
    pub fn record_circuit_breaker_transition(&self, host: &str, from: &str, to: &str) {
        self.add(
            CIRCUIT_BREAKER_TRANSITIONS_TOTAL,
            &[
                ("host", Some(host.to_owned())),
                ("from_state", Some(from.to_owned())),
                ("to_state", Some(to.to_owned())),
            ],
            1.0,
        );
    }

    /// The series a family currently holds, as rendered label sets.
    #[must_use]
    pub fn series(&self, family: &str) -> Vec<Vec<(String, String)>> {
        let index = Self::index_of(family);
        let mut series: Vec<Vec<(String, String)>> = if family_kind(family) == FamilyKind::Histogram
        {
            self.families[index].histogram.iter().map(|entry| entry.key().clone()).collect()
        } else {
            self.families[index].scalar.iter().map(|entry| entry.key().clone()).collect()
        };
        // A scrape order is deterministic, so a test and a dashboard see the
        // same sequence on every pull (`inst-os-scrape-6`).
        series.sort();
        series
    }

    /// The current value of one scalar series.
    #[must_use]
    pub fn value(&self, family: &str, labels: &[(&str, &str)]) -> Option<f64> {
        let index = Self::index_of(family);
        let mut matches: Vec<(Vec<(String, String)>, f64)> = self.families[index]
            .scalar
            .iter()
            .filter(|entry| {
                labels.iter().all(|(name, value)| {
                    entry.key().iter().any(|(key, key_value)| key == name && key_value == value)
                })
            })
            .map(|entry| (entry.key().clone(), *entry.value()))
            .collect();
        matches.sort_by(|left, right| left.0.cmp(&right.0));
        matches.into_iter().next().map(|(_, value)| value)
    }

    /// The Prometheus text exposition of every registered family
    /// (`inst-os-scrape-5`, `inst-os-scrape-6`).
    #[must_use]
    pub fn exposition(&self) -> String {
        let mut rendered = String::new();
        for (index, family) in FAMILIES.iter().enumerate() {
            let help = if *family == CIRCUIT_BREAKER_TRANSITIONS_TOTAL {
                "Circuit breaker transitions (registered; the breaker itself is future development)"
            } else {
                "OAGW metric"
            };
            rendered.push_str(&format!("# HELP {family} {help}\n"));
            rendered.push_str(&format!("# TYPE {family} {}\n", type_of_family(family)));
            match family_kind(family) {
                FamilyKind::Histogram => {
                    let mut series: Vec<_> = self.families[index]
                        .histogram
                        .iter()
                        .map(|entry| (entry.key().clone(), entry.value().clone()))
                        .collect();
                    series.sort_by(|left, right| left.0.cmp(&right.0));
                    for (key, accumulator) in series {
                        for (bound, count) in DURATION_BUCKETS.iter().zip(accumulator.buckets) {
                            rendered.push_str(&render_series(
                                family,
                                "_bucket",
                                &key,
                                Some(&bound.to_string()),
                                count as f64,
                            ));
                        }
                        rendered.push_str(&render_series(
                            family,
                            "_bucket",
                            &key,
                            Some("+Inf"),
                            accumulator.count as f64,
                        ));
                        rendered.push_str(&render_series(family, "_sum", &key, None, accumulator.sum));
                        rendered.push_str(&render_series(
                            family,
                            "_count",
                            &key,
                            None,
                            accumulator.count as f64,
                        ));
                    }
                }
                FamilyKind::Counter | FamilyKind::Gauge => {
                    let mut series: Vec<_> = self.families[index]
                        .scalar
                        .iter()
                        .map(|entry| (entry.key().clone(), *entry.value()))
                        .collect();
                    series.sort_by(|left, right| left.0.cmp(&right.0));
                    for (key, value) in series {
                        rendered.push_str(&render_series(family, "", &key, None, value));
                    }
                }
            }
        }
        rendered
    }
}

fn type_of_family(family: &str) -> &'static str {
    match family_kind(family) {
        FamilyKind::Counter => "counter",
        FamilyKind::Gauge => "gauge",
        FamilyKind::Histogram => "histogram",
    }
}

fn unit_of(family: &str) -> String {
    if family == REQUEST_DURATION_SECONDS {
        "s".to_owned()
    } else {
        String::new()
    }
}

/// The numeric form of a breaker state, for the gauge that registers it.
fn state_value(state: &str) -> f64 {
    match state {
        "closed" => 0.0,
        "open" => 1.0,
        "half_open" => 2.0,
        _ => 0.0,
    }
}

fn attributes(key: &[(String, String)]) -> Vec<KeyValue> {
    key.iter().map(|(name, value)| KeyValue::new(name.clone(), value.clone())).collect()
}

/// Render one series line, escaping the label values the format requires.
/// One rendered sample: `family[<suffix>]{labels[, le=bound]} value`.
fn render_series(
    family: &str,
    suffix: &str,
    labels: &[(String, String)],
    upper_bound: Option<&str>,
    value: f64,
) -> String {
    let mut rendered = String::from(family);
    rendered.push_str(suffix);
    if !labels.is_empty() || upper_bound.is_some() {
        rendered.push('{');
        let mut first = true;
        for (name, value) in labels {
            if !first {
                rendered.push(',');
            }
            first = false;
            rendered.push_str(name);
            rendered.push_str("=\"");
            rendered.push_str(&escape(value));
            rendered.push('"');
        }
        if let Some(bound) = upper_bound {
            if !first {
                rendered.push(',');
            }
            rendered.push_str("le=\"");
            rendered.push_str(bound);
            rendered.push('"');
        }
        rendered.push('}');
    }
    rendered.push(' ');
    rendered.push_str(&format_value(value));
    rendered.push('\n');
    rendered
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn format_value(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1.0e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

#[cfg(test)]
#[path = "metrics_tests.rs"]
mod metrics_tests;
