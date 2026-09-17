//! OAGW metrics registry (feature `cpt-cf-oagw-feature-observability-audit`,
//! DESIGN §4.2 Metrics and Observability; DoD
//! `cpt-cf-oagw-dod-observability-audit-metrics`).
//!
//! A self-contained, lock-free-on-the-hot-path registry exposing the DESIGN
//! metric vocabulary verbatim and rendering it in Prometheus text format for
//! the admin `/metrics` surface (algorithm
//! `cpt-cf-oagw-algo-observability-audit-cardinality`):
//!
//! | Family | Kind | Labels |
//! |--------|------|--------|
//! | `oagw_requests_total` | counter | `host`, `http.request.method`, `http.route`, `http.response.status_code` |
//! | `oagw_request_duration_seconds` | histogram | `host`, `http.route`, `phase` |
//! | `oagw_requests_in_flight` | gauge | `host` |
//! | `oagw_errors_total` | counter | `host`, `http.route`, `error_type`, `error_source` |
//! | `oagw_circuit_breaker_state` | gauge | `host` |
//! | `oagw_circuit_breaker_transitions_total` | counter | `host`, `from_state`, `to_state` |
//! | `oagw_rate_limit_exceeded_total` | counter | `host`, `path` |
//! | `oagw_rate_limit_usage_ratio` | gauge | `host`, `path` |
//! | `oagw_routing_target_host_used` | counter | `upstream_id`, `endpoint_host` |
//! | `oagw_routing_endpoint_selected` | counter | `upstream_id`, `endpoint_host`, `selection_method` |
//! | `oagw_upstream_available` | gauge | `host`, `endpoint` |
//! | `oagw_upstream_connections` | gauge | `host`, `state` |
//!
//! Cardinality controls (algorithm `cpt-cf-oagw-algo-observability-audit-cardinality`):
//! - No family accepts tenant-, principal-, or request-identifying labels
//!   (`inst-ob-card-reject`);
//! - each family bounds its label-combination series at
//!   [`DEFAULT_CARDINALITY_LIMIT`] (`inst-ob-card-drop`); observations beyond
//!   the cap are dropped and accounted in the `oagw_metrics_dropped_total`
//!   counter (`inst-ob-card-return`).
//!
//! Recording is a short shard-lock lookup (DashMap) followed by atomic
//! increments — no I/O, no cross-map reference holding, and none of the
//! hot-path instrumentation blocks or allocates beyond building the small
//! label set (the proxy pipeline allocates comparable per-request
//! structures already; acceptance `cpt-cf-oagw-nfr-observability`).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;

/// Histogram buckets for `oagw_request_duration_seconds` (DESIGN §4.2,
/// seconds).
pub const DURATION_BUCKETS: [f64; 12] = [
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Default maximum number of label-combination series per family.
pub const DEFAULT_CARDINALITY_LIMIT: usize = 512;

// --- Metric family names (verbatim from DESIGN §4.2) ---

/// `oagw_requests_total{host, http.request.method, http.route, http.response.status_code}`.
pub const REQUESTS_TOTAL: &str = "oagw_requests_total";
/// `oagw_request_duration_seconds{host, http.route, phase}`.
pub const REQUEST_DURATION: &str = "oagw_request_duration_seconds";
/// `oagw_requests_in_flight{host}`.
pub const REQUESTS_IN_FLIGHT: &str = "oagw_requests_in_flight";
/// `oagw_errors_total{host, http.route, error_type, error_source}`.
pub const ERRORS_TOTAL: &str = "oagw_errors_total";
/// `oagw_circuit_breaker_state{host}`.
pub const CIRCUIT_STATE: &str = "oagw_circuit_breaker_state";
/// `oagw_circuit_breaker_transitions_total{host, from_state, to_state}`.
pub const CIRCUIT_TRANSITIONS: &str = "oagw_circuit_breaker_transitions_total";
/// `oagw_rate_limit_exceeded_total{host, path}`.
pub const RATE_LIMIT_EXCEEDED: &str = "oagw_rate_limit_exceeded_total";
/// `oagw_rate_limit_usage_ratio{host, path}`.
pub const RATE_LIMIT_USAGE: &str = "oagw_rate_limit_usage_ratio";
/// `oagw_routing_target_host_used{upstream_id, endpoint_host}`.
pub const TARGET_HOST_USED: &str = "oagw_routing_target_host_used";
/// `oagw_routing_endpoint_selected{upstream_id, endpoint_host, selection_method}`.
pub const ENDPOINT_SELECTED: &str = "oagw_routing_endpoint_selected";
/// `oagw_upstream_available{host, endpoint}`.
pub const UPSTREAM_AVAILABLE: &str = "oagw_upstream_available";
/// `oagw_upstream_connections{host, state}`.
pub const UPSTREAM_CONNECTIONS: &str = "oagw_upstream_connections";
/// Cardinality-drop accounting counter (algorithm
/// `cpt-cf-oagw-algo-observability-audit-cardinality`, `inst-ob-card-return`).
pub const DROPPED_TOTAL: &str = "oagw_metrics_dropped_total";

/// Normalizes an HTTP method to an OTel-semconv standard verb or `_OTHER`
/// (bounded `http.request.method` label value — DESIGN §4.2).
#[must_use]
pub fn normalize_method(method: &str) -> &'static str {
    match method.to_ascii_uppercase().as_str() {
        "GET" => "GET",
        "POST" => "POST",
        "PUT" => "PUT",
        "DELETE" => "DELETE",
        "PATCH" => "PATCH",
        "HEAD" => "HEAD",
        "OPTIONS" => "OPTIONS",
        "CONNECT" => "CONNECT",
        "TRACE" => "TRACE",
        _ => "_OTHER",
    }
}

/// A sorted label vector (deterministic exposition + dedup of equivalent
/// label orders).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct LabelKey(Vec<(String, String)>);

impl LabelKey {
    fn of(labels: &[(&str, &str)]) -> Self {
        let mut pairs: Vec<(String, String)> = labels
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        pairs.sort();
        Self(pairs)
    }
}

#[derive(Debug, Default)]
struct CounterCell {
    value: AtomicU64,
}

#[derive(Debug, Default)]
struct GaugeCell {
    /// `f64::to_bits` — atomic float gauge.
    bits: AtomicU64,
}

impl GaugeCell {
    fn set(&self, value: f64) {
        self.bits.store(value.to_bits(), Ordering::Relaxed);
    }

    fn add(&self, delta: f64) {
        let mut current = self.bits.load(Ordering::Relaxed);
        loop {
            let next = (f64::from_bits(current) + delta).to_bits();
            match self.bits.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }

    fn get(&self) -> f64 {
        f64::from_bits(self.bits.load(Ordering::Relaxed))
    }
}

/// A cumulative histogram cell: `buckets[i]` counts observations `<=`
/// `DURATION_BUCKETS[i]`; `+Inf` is the total count.
#[derive(Debug)]
struct HistogramCell {
    buckets: Box<[AtomicU64]>,
    count: AtomicU64,
    sum_nanos: AtomicU64,
}

impl Default for HistogramCell {
    fn default() -> Self {
        let buckets = DURATION_BUCKETS
            .iter()
            .map(|_| AtomicU64::new(0))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            buckets,
            count: AtomicU64::new(0),
            sum_nanos: AtomicU64::new(0),
        }
    }
}

/// Shared counter family (bounded series + drop accounting).
#[derive(Debug)]
struct CounterFamily {
    name: &'static str,
    help: &'static str,
    series: DashMap<LabelKey, Arc<CounterCell>>,
    cap: usize,
    dropped: AtomicU64,
}

impl CounterFamily {
    fn new(name: &'static str, help: &'static str, cap: usize) -> Self {
        Self {
            name,
            help,
            series: DashMap::new(),
            cap,
            dropped: AtomicU64::new(0),
        }
    }

    fn cell(&self, key: &LabelKey) -> Option<Arc<CounterCell>> {
        if let Some(cell) = self.series.get(key) {
            return Some(cell.clone());
        }
        if self.series.len() >= self.cap {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        // Return the *stored* cell: concurrent first observations of the same
        // key must all increment the same series (the `entry` insert is the
        // single winner; a pre-built loser cell is dropped).
        Some(
            self.series
                .entry(key.clone())
                .or_insert_with(|| Arc::new(CounterCell::default()))
                .clone(),
        )
    }

    fn inc(&self, key: &LabelKey) {
        if let Some(cell) = self.cell(key) {
            cell.value.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn value(&self, key: &LabelKey) -> u64 {
        self.series
            .get(key)
            .map(|c| c.value.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    fn len(&self) -> usize {
        self.series.len()
    }

    fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Shared gauge family (bounded series + drop accounting).
#[derive(Debug)]
struct GaugeFamily {
    name: &'static str,
    help: &'static str,
    series: DashMap<LabelKey, Arc<GaugeCell>>,
    cap: usize,
    dropped: AtomicU64,
}

impl GaugeFamily {
    fn new(name: &'static str, help: &'static str, cap: usize) -> Self {
        Self {
            name,
            help,
            series: DashMap::new(),
            cap,
            dropped: AtomicU64::new(0),
        }
    }

    fn cell(&self, key: &LabelKey) -> Option<Arc<GaugeCell>> {
        if let Some(cell) = self.series.get(key) {
            return Some(cell.clone());
        }
        if self.series.len() >= self.cap {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        // Return the *stored* cell (see `CounterFamily::cell`).
        Some(
            self.series
                .entry(key.clone())
                .or_insert_with(|| Arc::new(GaugeCell::default()))
                .clone(),
        )
    }

    fn set(&self, key: &LabelKey, value: f64) {
        if let Some(cell) = self.cell(key) {
            cell.set(value);
        }
    }

    fn add(&self, key: &LabelKey, delta: f64) {
        if let Some(cell) = self.cell(key) {
            cell.add(delta);
        }
    }

    fn value(&self, key: &LabelKey) -> Option<f64> {
        self.series.get(key).map(|c| c.get())
    }

    fn len(&self) -> usize {
        self.series.len()
    }

    fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Shared histogram family (bounded series + drop accounting).
#[derive(Debug)]
struct HistogramFamily {
    name: &'static str,
    help: &'static str,
    series: DashMap<LabelKey, Arc<HistogramCell>>,
    cap: usize,
    dropped: AtomicU64,
}

impl HistogramFamily {
    fn new(name: &'static str, help: &'static str, cap: usize) -> Self {
        Self {
            name,
            help,
            series: DashMap::new(),
            cap,
            dropped: AtomicU64::new(0),
        }
    }

    fn cell(&self, key: &LabelKey) -> Option<Arc<HistogramCell>> {
        if let Some(cell) = self.series.get(key) {
            return Some(cell.clone());
        }
        if self.series.len() >= self.cap {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        // Return the *stored* cell (see `CounterFamily::cell`).
        Some(
            self.series
                .entry(key.clone())
                .or_insert_with(|| Arc::new(HistogramCell::default()))
                .clone(),
        )
    }

    fn observe(&self, key: &LabelKey, seconds: f64) {
        let Some(cell) = self.cell(key) else {
            return;
        };
        let seconds = seconds.max(0.0);
        let nanos = (seconds * 1_000_000_000.0) as u64;
        // Cumulative semantics: an observation counts toward every bucket
        // whose upper bound is at or above the value (`le`), so buckets
        // `i >= idx` (the first bucket not below the value) are incremented;
        // a value beyond the largest bound increments none (the `+Inf`
        // bucket is the total count).
        let idx = DURATION_BUCKETS
            .iter()
            .position(|b| seconds <= *b)
            .unwrap_or(DURATION_BUCKETS.len());
        for (i, bucket) in cell.buckets.iter().enumerate() {
            if i >= idx {
                bucket.fetch_add(1, Ordering::Relaxed);
            }
        }
        cell.count.fetch_add(1, Ordering::Relaxed);
        cell.sum_nanos.fetch_add(nanos, Ordering::Relaxed);
    }

    fn count(&self, key: &LabelKey) -> u64 {
        self.series
            .get(key)
            .map(|c| c.count.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    fn sum_nanos(&self, key: &LabelKey) -> u64 {
        self.series
            .get(key)
            .map(|c| c.sum_nanos.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    fn len(&self) -> usize {
        self.series.len()
    }

    fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// The OAGW metrics registry — the 12 DESIGN families plus the
/// cardinality-drop counter.  Created once per gear process and shared
/// between the planes and the `/metrics` admin surface.
#[derive(Debug)]
pub struct MetricsRegistry {
    requests: CounterFamily,
    duration: HistogramFamily,
    in_flight: GaugeFamily,
    errors: CounterFamily,
    circuit_state: GaugeFamily,
    circuit_transitions: CounterFamily,
    rate_limit_exceeded: CounterFamily,
    rate_limit_usage: GaugeFamily,
    target_host_used: CounterFamily,
    endpoint_selected: CounterFamily,
    upstream_available: GaugeFamily,
    upstream_connections: GaugeFamily,
}

impl Default for MetricsRegistry {
    fn default() -> Self {
        Self::new(DEFAULT_CARDINALITY_LIMIT)
    }
}

impl MetricsRegistry {
    /// Creates the registry with the per-family cardinality cap.
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            requests: CounterFamily::new(
                REQUESTS_TOTAL,
                "Proxy requests, labeled by host (upstream alias), method, route and status",
                cap,
            ),
            duration: HistogramFamily::new(
                REQUEST_DURATION,
                "Proxy request duration in seconds by host, route and phase",
                cap,
            ),
            in_flight: GaugeFamily::new(
                REQUESTS_IN_FLIGHT,
                "In-flight proxy requests by host",
                cap,
            ),
            errors: CounterFamily::new(
                ERRORS_TOTAL,
                "Proxy errors by host, route, error type and error source (ADR 0007)",
                cap,
            ),
            circuit_state: GaugeFamily::new(
                CIRCUIT_STATE,
                "Circuit-breaker state per host (0 closed, 0.5 half-open, 1 open)",
                cap,
            ),
            circuit_transitions: CounterFamily::new(
                CIRCUIT_TRANSITIONS,
                "Circuit-breaker state transitions per host",
                cap,
            ),
            rate_limit_exceeded: CounterFamily::new(
                RATE_LIMIT_EXCEEDED,
                "Requests rejected by rate limiting per host and path",
                cap,
            ),
            rate_limit_usage: GaugeFamily::new(
                RATE_LIMIT_USAGE,
                "Rate-limit usage ratio per host and path, bounded [0.0, 1.0]",
                cap,
            ),
            target_host_used: CounterFamily::new(
                TARGET_HOST_USED,
                "Requests selecting an endpoint via the X-OAGW-Target-Host header (ADR 0001)",
                cap,
            ),
            endpoint_selected: CounterFamily::new(
                ENDPOINT_SELECTED,
                "Endpoint selections by method: explicit_header, round_robin, default",
                cap,
            ),
            upstream_available: GaugeFamily::new(
                UPSTREAM_AVAILABLE,
                "Upstream availability per host and endpoint (0 down, 1 up)",
                cap,
            ),
            upstream_connections: GaugeFamily::new(
                UPSTREAM_CONNECTIONS,
                "Upstream connection pool state per host (idle, active, max)",
                cap,
            ),
        }
    }

    /// Records a completed request (`inst-ob-rr-requests`).
    #[allow(clippy::too_many_arguments)]
    pub fn record_request(&self, host: &str, method: &str, route: &str, status: u16) {
        let labels = LabelKey::of(&[
            ("host", host),
            ("http.request.method", normalize_method(method)),
            ("http.route", route),
            ("http.response.status_code", &status.to_string()),
        ]);
        self.requests.inc(&labels);
    }

    /// Observes the full-request duration (`inst-ob-rr-duration`; the `phase`
    /// label is recorded as `total` for the whole proxy request).
    pub fn observe_duration(&self, host: &str, route: &str, phase: &str, seconds: f64) {
        let labels = LabelKey::of(&[("host", host), ("http.route", route), ("phase", phase)]);
        self.duration.observe(&labels, seconds);
    }

    /// Adjusts the in-flight gauge (`inst-ob-rr-inflight`; `+1` at request
    /// entry, `-1` on completion — see [`InFlightGuard`]).
    pub fn in_flight_add(&self, host: &str, delta: i64) {
        let labels = LabelKey::of(&[("host", host)]);
        self.in_flight.add(&labels, delta as f64);
    }

    /// Records an errored request (`inst-ob-rr-errors`), labeled by error
    /// type (the GTS instance for gateway errors, or a bounded upstream
    /// marker) and error source (`gateway` vs `upstream`, ADR 0007 / DoD
    /// `cpt-cf-oagw-dod-observability-audit-error-source`).
    pub fn record_error(&self, host: &str, route: &str, error_type: &str, source: &str) {
        let labels = LabelKey::of(&[
            ("host", host),
            ("http.route", route),
            ("error_type", error_type),
            ("error_source", source),
        ]);
        self.errors.inc(&labels);
    }

    /// Increments `oagw_rate_limit_exceeded_total` (`inst-ob-rl-exceeded`).
    pub fn record_rate_limit_exceeded(&self, host: &str, path: &str) {
        let labels = LabelKey::of(&[("host", host), ("path", path)]);
        self.rate_limit_exceeded.inc(&labels);
    }

    /// Observes the rate-limit usage ratio (`inst-ob-rl-ratio`; bounded
    /// [0.0, 1.0] — the canonical gauge bound, docs DESIGN §4.2: 1.0 means
    /// the bucket is exhausted).
    pub fn observe_rate_limit_usage(&self, host: &str, path: &str, ratio: f64) {
        let labels = LabelKey::of(&[("host", host), ("path", path)]);
        self.rate_limit_usage.set(&labels, ratio.clamp(0.0, 1.0));
    }

    /// Sets the circuit-breaker state gauge (`inst-ob-cbm-*`; 0 closed,
    /// 0.5 half-open, 1 open).
    pub fn record_circuit_state(&self, host: &str, state: f64) {
        let labels = LabelKey::of(&[("host", host)]);
        self.circuit_state.set(&labels, state);
    }

    /// Increments the circuit-breaker transition counter
    /// (`inst-ob-ru-cb`).
    pub fn record_circuit_transition(&self, host: &str, from: &str, to: &str) {
        let labels = LabelKey::of(&[("host", host), ("from_state", from), ("to_state", to)]);
        self.circuit_transitions.inc(&labels);
    }

    /// Increments `oagw_routing_target_host_used` when the
    /// `X-OAGW-Target-Host` header selected the endpoint (`inst-ob-ru-target`).
    pub fn record_target_host_used(&self, upstream_id: &uuid::Uuid, endpoint_host: &str) {
        let labels = LabelKey::of(&[
            ("upstream_id", &upstream_id.to_string()),
            ("endpoint_host", endpoint_host),
        ]);
        self.target_host_used.inc(&labels);
    }

    /// Records an endpoint selection with its method (`inst-ob-ru-endpoint`):
    /// `explicit_header`, `round_robin`, or `default`.
    pub fn record_endpoint_selected(
        &self,
        upstream_id: &uuid::Uuid,
        endpoint_host: &str,
        selection_method: &str,
    ) {
        let labels = LabelKey::of(&[
            ("upstream_id", &upstream_id.to_string()),
            ("endpoint_host", endpoint_host),
            ("selection_method", selection_method),
        ]);
        self.endpoint_selected.inc(&labels);
    }

    /// Tracks upstream availability (`inst-ob-ru-up`: 1 after a successful
    /// response, 0 after a link failure).
    pub fn record_upstream_available(&self, host: &str, endpoint: &str, available: bool) {
        let labels = LabelKey::of(&[("host", host), ("endpoint", endpoint)]);
        self.upstream_available
            .set(&labels, if available { 1.0 } else { 0.0 });
    }

    /// Tracks upstream connection-pool state (`inst-ob-ru-up`; `state` is
    /// `idle`/`active`/`max`).  Recorded on demand — the legacy hyper client
    /// does not expose pool introspection, so the hook is the vocabulary
    /// surface for pool-adapter futures.
    pub fn record_upstream_connections(&self, host: &str, state: &str, count: i64) {
        let labels = LabelKey::of(&[("host", host), ("state", state)]);
        self.upstream_connections.set(&labels, count as f64);
    }

    // --- Test/introspection surface ---

    /// The counter value for one series, or 0 when absent.
    #[must_use]
    pub fn counter_value(&self, family: &str, labels: &[(&str, &str)]) -> u64 {
        let key = LabelKey::of(labels);
        match family {
            REQUESTS_TOTAL => self.requests.value(&key),
            ERRORS_TOTAL => self.errors.value(&key),
            RATE_LIMIT_EXCEEDED => self.rate_limit_exceeded.value(&key),
            CIRCUIT_TRANSITIONS => self.circuit_transitions.value(&key),
            TARGET_HOST_USED => self.target_host_used.value(&key),
            ENDPOINT_SELECTED => self.endpoint_selected.value(&key),
            _ => 0,
        }
    }

    /// The gauge value for one series, or None when absent.
    #[must_use]
    pub fn gauge_value(&self, family: &str, labels: &[(&str, &str)]) -> Option<f64> {
        let key = LabelKey::of(labels);
        match family {
            REQUESTS_IN_FLIGHT => self.in_flight.value(&key),
            RATE_LIMIT_USAGE => self.rate_limit_usage.value(&key),
            CIRCUIT_STATE => self.circuit_state.value(&key),
            UPSTREAM_AVAILABLE => self.upstream_available.value(&key),
            UPSTREAM_CONNECTIONS => self.upstream_connections.value(&key),
            _ => None,
        }
    }

    /// The histogram observation count for one series, or 0 when absent.
    #[must_use]
    pub fn histogram_count(&self, labels: &[(&str, &str)]) -> u64 {
        let key = LabelKey::of(labels);
        self.duration.count(&key)
    }

    /// The histogram sum (nanoseconds) for one series.
    #[must_use]
    pub fn histogram_sum_nanos(&self, labels: &[(&str, &str)]) -> u64 {
        let key = LabelKey::of(labels);
        self.duration.sum_nanos(&key)
    }

    /// The number of live label-combination series in one family (the
    /// cardinality bound check).
    #[must_use]
    pub fn family_series_count(&self, family: &str) -> usize {
        match family {
            REQUESTS_TOTAL => self.requests.len(),
            REQUEST_DURATION => self.duration.len(),
            REQUESTS_IN_FLIGHT => self.in_flight.len(),
            ERRORS_TOTAL => self.errors.len(),
            CIRCUIT_STATE => self.circuit_state.len(),
            CIRCUIT_TRANSITIONS => self.circuit_transitions.len(),
            RATE_LIMIT_EXCEEDED => self.rate_limit_exceeded.len(),
            RATE_LIMIT_USAGE => self.rate_limit_usage.len(),
            TARGET_HOST_USED => self.target_host_used.len(),
            ENDPOINT_SELECTED => self.endpoint_selected.len(),
            UPSTREAM_AVAILABLE => self.upstream_available.len(),
            UPSTREAM_CONNECTIONS => self.upstream_connections.len(),
            _ => 0,
        }
    }

    /// The total number of observations dropped by the cardinality guard
    /// across every family (the sum of the per-family drop counters).
    #[must_use]
    pub fn dropped_total(&self) -> u64 {
        self.requests.dropped()
            + self.duration.dropped()
            + self.in_flight.dropped()
            + self.errors.dropped()
            + self.circuit_state.dropped()
            + self.circuit_transitions.dropped()
            + self.rate_limit_exceeded.dropped()
            + self.rate_limit_usage.dropped()
            + self.target_host_used.dropped()
            + self.endpoint_selected.dropped()
            + self.upstream_available.dropped()
            + self.upstream_connections.dropped()
    }

    /// Renders every family in Prometheus text exposition format
    /// (`inst-ob-scrape-cols`, `inst-ob-scrape-return`).  Series are emitted
    /// in sorted label order for deterministic output; the histogram
    /// cumulative bucket lines close with the `+Inf` bucket.
    #[must_use]
    pub fn render_prometheus(&self) -> String {
        let mut out = String::new();
        let mut dropped_lines = Vec::new();

        let counters = vec![
            &self.requests,
            &self.errors,
            &self.circuit_transitions,
            &self.rate_limit_exceeded,
            &self.target_host_used,
            &self.endpoint_selected,
        ];
        for family in counters {
            Self::render_counter(&mut out, family);
            if family.dropped() > 0 {
                dropped_lines.push(format!(
                    "{}{{family=\"{}\"}} {}",
                    DROPPED_TOTAL,
                    family.name,
                    family.dropped()
                ));
            }
        }

        let gauges = vec![
            &self.in_flight,
            &self.circuit_state,
            &self.rate_limit_usage,
            &self.upstream_available,
            &self.upstream_connections,
        ];
        for family in gauges {
            Self::render_gauge(&mut out, family);
            if family.dropped() > 0 {
                dropped_lines.push(format!(
                    "{}{{family=\"{}\"}} {}",
                    DROPPED_TOTAL,
                    family.name,
                    family.dropped()
                ));
            }
        }

        Self::render_histogram(&mut out, &self.duration);
        if self.duration.dropped() > 0 {
            dropped_lines.push(format!(
                "{}{{family=\"{}\"}} {}",
                DROPPED_TOTAL,
                self.duration.name,
                self.duration.dropped()
            ));
        }

        // The unlabeled drop counter always renders so the guard is visible
        // (plus a per-family line for any family that hit the cap).
        out.push_str(&format!(
            "# HELP {DROPPED_TOTAL} Observations dropped by the cardinality guard\n"
        ));
        out.push_str(&format!("# TYPE {DROPPED_TOTAL} counter\n"));
        out.push_str(&format!("{DROPPED_TOTAL} {}\n", self.dropped_total()));
        for line in dropped_lines {
            out.push_str(&line);
            out.push('\n');
        }
        out
    }

    fn render_counter(out: &mut String, family: &CounterFamily) {
        out.push_str(&format!("# HELP {} {}\n", family.name, family.help));
        out.push_str(&format!("# TYPE {} counter\n", family.name));
        let mut keys: Vec<LabelKey> = family.series.iter().map(|e| e.key().clone()).collect();
        keys.sort();
        for key in keys {
            let value = family.value(&key);
            out.push_str(&format!(
                "{}{{{}}} {}\n",
                family.name,
                render_labels(&key),
                value
            ));
        }
    }

    fn render_gauge(out: &mut String, family: &GaugeFamily) {
        out.push_str(&format!("# HELP {} {}\n", family.name, family.help));
        out.push_str(&format!("# TYPE {} gauge\n", family.name));
        let mut keys: Vec<LabelKey> = family.series.iter().map(|e| e.key().clone()).collect();
        keys.sort();
        for key in keys {
            let value = family.value(&key).unwrap_or(0.0);
            out.push_str(&format!(
                "{}{{{}}} {}\n",
                family.name,
                render_labels(&key),
                value
            ));
        }
    }

    fn render_histogram(out: &mut String, family: &HistogramFamily) {
        out.push_str(&format!("# HELP {} {}\n", family.name, family.help));
        out.push_str(&format!("# TYPE {} histogram\n", family.name));
        let mut keys: Vec<LabelKey> = family.series.iter().map(|e| e.key().clone()).collect();
        keys.sort();
        for key in keys {
            let Some(cell) = family.series.get(&key) else {
                continue;
            };
            for (i, bucket) in cell.buckets.iter().enumerate() {
                let le = format!("{}", DURATION_BUCKETS[i]);
                let value = bucket.load(Ordering::Relaxed);
                out.push_str(&format!(
                    "{}_bucket{{{},le=\"{}\"}} {}\n",
                    family.name,
                    render_labels(&key),
                    le,
                    value
                ));
            }
            let count = cell.count.load(Ordering::Relaxed);
            // The `+Inf` bucket is the total observation count.
            out.push_str(&format!(
                "{}_bucket{{{},le=\"+Inf\"}} {}\n",
                family.name,
                render_labels(&key),
                count
            ));
            let sum = cell.sum_nanos.load(Ordering::Relaxed) as f64 / 1_000_000_000.0;
            out.push_str(&format!(
                "{}_sum{{{}}} {}\n",
                family.name,
                render_labels(&key),
                sum
            ));
            out.push_str(&format!(
                "{}_count{{{}}} {}\n",
                family.name,
                render_labels(&key),
                count
            ));
        }
    }
}

/// Renders the label set **body** (`k="v",k2="v2"`, without the surrounding
/// braces — the call sites wrap it) from a sorted label key with Prometheus
/// escaping for `\`, `"` and newlines.
fn render_labels(key: &LabelKey) -> String {
    if key.0.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for (i, (name, value)) in key.0.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(name);
        out.push_str("=\"");
        for ch in value.chars() {
            match ch {
                '\\' => out.push_str("\\\\"),
                '"' => out.push_str("\\\""),
                '\n' => out.push_str("\\n"),
                c => out.push(c),
            }
        }
        out.push('"');
    }
    out
}

/// Drop guard decrementing the in-flight gauge when the proxy request
/// completes or the future is dropped (panic-cancellation-safe).
pub struct InFlightGuard {
    registry: Arc<MetricsRegistry>,
    host: String,
}

impl InFlightGuard {
    /// Increments the in-flight gauge for `host` and returns the guard whose
    /// `Drop` decrements it again (`inst-ob-rr-inflight`).
    #[must_use]
    pub fn enter(registry: &Arc<MetricsRegistry>, host: String) -> Self {
        registry.in_flight_add(&host, 1);
        Self {
            registry: Arc::clone(registry),
            host,
        }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.registry.in_flight_add(&self.host, -1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(host: &str, status: u16) -> Vec<(&'static str, String)> {
        vec![
            ("host", host.to_owned()),
            ("http.request.method", "GET".to_owned()),
            ("http.route", "/v1".to_owned()),
            ("http.response.status_code", status.to_string()),
        ]
    }

    fn label_refs<'a>(labels: &'a [(&'static str, String)]) -> Vec<(&'a str, &'a str)> {
        labels.iter().map(|(k, v)| (*k, v.as_str())).collect()
    }

    #[test]
    fn method_is_normalized_to_standard_verbs() {
        assert_eq!(normalize_method("get"), "GET");
        assert_eq!(normalize_method("PATCH"), "PATCH");
        assert_eq!(normalize_method("PROPFIND"), "_OTHER");
    }

    #[test]
    fn request_counter_moves_per_status_series() {
        let r = MetricsRegistry::default();
        let l1 = labels("svc", 200);
        let l2 = labels("svc", 404);
        r.record_request("svc", "GET", "/v1", 200);
        r.record_request("svc", "GET", "/v1", 200);
        r.record_request("svc", "GET", "/v1", 404);
        assert_eq!(
            r.counter_value(REQUESTS_TOTAL, &label_refs(&l1)),
            2,
            "200 series counts both 200s"
        );
        assert_eq!(
            r.counter_value(REQUESTS_TOTAL, &label_refs(&l2)),
            1,
            "404 series counts the 404"
        );
        // Status class queries are expressed at query time; series are per
        // numeric status.
        assert_eq!(r.family_series_count(REQUESTS_TOTAL), 2);
    }

    #[test]
    fn errors_counter_moves_for_gateway_and_upstream_sources() {
        let r = MetricsRegistry::default();
        r.record_error(
            "svc",
            "/v1",
            "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1",
            "gateway",
        );
        r.record_error("svc", "/v1", "upstream_http_error", "upstream");
        r.record_error("svc", "/v1", "upstream_http_error", "upstream");
        assert_eq!(
            r.counter_value(
                ERRORS_TOTAL,
                &[
                    ("host", "svc"),
                    ("http.route", "/v1"),
                    (
                        "error_type",
                        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
                    ),
                    ("error_source", "gateway"),
                ]
            ),
            1
        );
        assert_eq!(
            r.counter_value(
                ERRORS_TOTAL,
                &[
                    ("host", "svc"),
                    ("http.route", "/v1"),
                    ("error_type", "upstream_http_error"),
                    ("error_source", "upstream"),
                ]
            ),
            2
        );
    }

    #[test]
    fn duration_histogram_records_count_sum_and_cumulative_buckets() {
        let r = MetricsRegistry::default();
        let k = [("host", "svc"), ("http.route", "/v1"), ("phase", "total")];
        r.observe_duration("svc", "/v1", "total", 0.004);
        r.observe_duration("svc", "/v1", "total", 0.02);
        assert_eq!(r.histogram_count(&k), 2);
        let sum = r.histogram_sum_nanos(&k) as f64 / 1_000_000_000.0;
        assert!((sum - 0.024).abs() < 0.000_001, "sum = {sum}");
        let text = r.render_prometheus();
        // 0.004 lands in the 0.005 cumulative bucket only; 0.02 lands in
        // 0.025 and above.  The +Inf bucket equals the count.
        assert!(text.contains("oagw_request_duration_seconds_bucket{host=\"svc\",http.route=\"/v1\",phase=\"total\",le=\"0.005\"} 1"));
        assert!(text.contains("oagw_request_duration_seconds_bucket{host=\"svc\",http.route=\"/v1\",phase=\"total\",le=\"0.05\"} 2"));
        assert!(text.contains("oagw_request_duration_seconds_bucket{host=\"svc\",http.route=\"/v1\",phase=\"total\",le=\"+Inf\"} 2"));
    }

    #[test]
    fn in_flight_gauge_returns_to_zero_via_guard() {
        let r = Arc::new(MetricsRegistry::default());
        // No series before the first observation (bounded exposition).
        assert_eq!(r.gauge_value(REQUESTS_IN_FLIGHT, &[("host", "svc")]), None);
        {
            let _guard = InFlightGuard {
                registry: Arc::clone(&r),
                host: "svc".to_owned(),
            };
            r.in_flight_add("svc", 1);
            assert_eq!(
                r.gauge_value(REQUESTS_IN_FLIGHT, &[("host", "svc")]),
                Some(1.0)
            );
        }
        assert_eq!(
            r.gauge_value(REQUESTS_IN_FLIGHT, &[("host", "svc")]),
            Some(0.0)
        );
    }

    #[test]
    fn rate_limit_exceeded_and_usage_ratio_are_recorded() {
        let r = MetricsRegistry::default();
        r.record_rate_limit_exceeded("svc", "/v1/models");
        r.record_rate_limit_exceeded("svc", "/v1/models");
        r.observe_rate_limit_usage("svc", "/v1/models", 0.75);
        assert_eq!(
            r.counter_value(
                RATE_LIMIT_EXCEEDED,
                &[("host", "svc"), ("path", "/v1/models")]
            ),
            2
        );
        assert_eq!(
            r.gauge_value(RATE_LIMIT_USAGE, &[("host", "svc"), ("path", "/v1/models")]),
            Some(0.75)
        );
    }

    #[test]
    fn rate_limit_usage_ratio_is_clamped_to_the_canonical_0_1_bound() {
        // F-014: the gauge bound is [0.0, 1.0] (docs DESIGN §4.2) — an
        // out-of-range projection must be clamped, never reported above 1.0.
        let r = MetricsRegistry::default();
        r.observe_rate_limit_usage("svc", "/v1/models", 2.0);
        r.observe_rate_limit_usage("svc", "/v1/deploy", -5.0);
        assert_eq!(
            r.gauge_value(RATE_LIMIT_USAGE, &[("host", "svc"), ("path", "/v1/models")]),
            Some(1.0),
            "over-budget ratio clamps to 1.0"
        );
        assert_eq!(
            r.gauge_value(RATE_LIMIT_USAGE, &[("host", "svc"), ("path", "/v1/deploy")]),
            Some(0.0),
            "negative ratio clamps to 0.0"
        );
    }

    #[test]
    fn concurrent_first_observations_all_count_toward_one_series() {
        // F-006: racing first observations of the same label key must all land
        // on the *stored* series — no detached cell may eat increments.
        let r = Arc::new(MetricsRegistry::default());
        const THREADS: usize = 16;
        const INC_PER_THREAD: u64 = 1_000;
        let mut handles = Vec::new();
        for _ in 0..THREADS {
            let r = Arc::clone(&r);
            handles.push(std::thread::spawn(move || {
                for _ in 0..INC_PER_THREAD {
                    r.record_request("svc", "GET", "/v1/models", 200);
                }
            }));
        }
        for h in handles {
            h.join().expect("thread joins");
        }
        assert_eq!(
            r.counter_value(
                REQUESTS_TOTAL,
                &[
                    ("host", "svc"),
                    ("http.request.method", "GET"),
                    ("http.route", "/v1/models"),
                    ("http.response.status_code", "200"),
                ]
            ),
            (THREADS as u64) * INC_PER_THREAD,
            "no observation may be lost to a detached cell"
        );
        // Single series held: the cardinality never overshoots cap for one key.
        assert_eq!(
            r.family_series_count(REQUESTS_TOTAL),
            1,
            "all counters collapse into one series per label set"
        );
    }

    #[test]
    fn routing_and_upstream_families_record() {
        let r = MetricsRegistry::default();
        let up = uuid::Uuid::from_u128(7);
        r.record_target_host_used(&up, "eu.vendor.com");
        r.record_endpoint_selected(&up, "eu.vendor.com", "explicit_header");
        r.record_endpoint_selected(&up, "192.0.2.1", "round_robin");
        r.record_upstream_available("vendor.com", "eu.vendor.com", true);
        r.record_upstream_connections("vendor.com", "active", 3);
        assert_eq!(
            r.counter_value(
                TARGET_HOST_USED,
                &[
                    ("upstream_id", &up.to_string()),
                    ("endpoint_host", "eu.vendor.com")
                ]
            ),
            1
        );
        assert_eq!(
            r.counter_value(
                ENDPOINT_SELECTED,
                &[
                    ("upstream_id", &up.to_string()),
                    ("endpoint_host", "eu.vendor.com"),
                    ("selection_method", "explicit_header"),
                ]
            ),
            1
        );
        assert_eq!(
            r.gauge_value(
                UPSTREAM_AVAILABLE,
                &[("host", "vendor.com"), ("endpoint", "eu.vendor.com")]
            ),
            Some(1.0)
        );
        assert_eq!(
            r.gauge_value(
                UPSTREAM_CONNECTIONS,
                &[("host", "vendor.com"), ("state", "active")]
            ),
            Some(3.0)
        );
    }

    #[test]
    fn circuit_breaker_state_and_transitions_record() {
        let r = MetricsRegistry::default();
        r.record_circuit_state("svc", 0.0);
        r.record_circuit_transition("svc", "closed", "open");
        r.record_circuit_state("svc", 1.0);
        r.record_circuit_transition("svc", "open", "half_open");
        assert_eq!(r.gauge_value(CIRCUIT_STATE, &[("host", "svc")]), Some(1.0));
        assert_eq!(
            r.counter_value(
                CIRCUIT_TRANSITIONS,
                &[
                    ("host", "svc"),
                    ("from_state", "closed"),
                    ("to_state", "open")
                ]
            ),
            1
        );
        assert_eq!(
            r.counter_value(
                CIRCUIT_TRANSITIONS,
                &[
                    ("host", "svc"),
                    ("from_state", "open"),
                    ("to_state", "half_open")
                ]
            ),
            1
        );
    }

    #[test]
    fn cardinality_guard_drops_beyond_the_cap() {
        let r = MetricsRegistry::new(3);
        for i in 0..6 {
            r.record_request(&format!("host{i}"), "GET", "/", 200);
        }
        assert_eq!(r.family_series_count(REQUESTS_TOTAL), 3, "bounded at cap");
        assert!(r.dropped_total() >= 3, "drops accounted");
        let text = r.render_prometheus();
        assert!(
            text.contains("oagw_metrics_dropped_total"),
            "drop counter renders"
        );
    }

    #[test]
    fn exposition_is_deterministic_and_escapes_labels() {
        let r = MetricsRegistry::default();
        r.record_request("svc", "GET", "/v1", 200);
        r.record_request("svc", "GET", "/v1", 200);
        let first = r.render_prometheus();
        let second = r.render_prometheus();
        assert_eq!(first, second, "deterministic");
        for family in [
            REQUESTS_TOTAL,
            ERRORS_TOTAL,
            REQUEST_DURATION,
            REQUESTS_IN_FLIGHT,
        ] {
            assert!(
                first.contains(&format!("# TYPE {family} ")),
                "family {family} declared"
            );
        }
        // Label keys are emitted in sorted order for deterministic exposition.
        assert!(first.contains("oagw_requests_total{host=\"svc\",http.request.method=\"GET\",http.response.status_code=\"200\",http.route=\"/v1\"} 2"));
        // A quote/value escape round-trips.
        let e = MetricsRegistry::default();
        e.observe_rate_limit_usage("a\"b", "c\\d", 1.0);
        let text = e.render_prometheus();
        assert!(text.contains("host=\"a\\\"b\""), "quote escaped");
        assert!(text.contains("path=\"c\\\\d\""), "backslash escaped");
    }
}
