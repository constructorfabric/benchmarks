//! Data-plane instrumentation (`DESIGN.md` § 4.2, FR-026).
//!
//! The gear owns typed OpenTelemetry instruments created from the meter the
//! host installs as the global provider, plus process-local mirrors of the
//! request counters so the gear's own tests and health surface can read totals
//! without an exporter. Recording is attributed with the label vocabulary the
//! design fixes — `host` is the upstream alias, the HTTP attributes follow the
//! OTel semantic conventions, and no tenant label is ever attached.
//!
//! Names are dotted here because that is how the rest of the platform names
//! instruments; the exporter renders them under the `oagw_*` Prometheus names
//! the design lists (`oagw.request.count` → `oagw_requests_total`).

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use dashmap::DashMap;
use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::{Counter, Gauge, Histogram, Meter};

/// The meter this gear creates its instruments from.
pub const METER_NAME: &str = "cf.gears.oagw";

// ─── Instrument names ────────────────────────────────────────────────

/// Counter: proxied requests by upstream, method, route and status.
pub const REQUESTS_TOTAL: &str = "oagw.request.count";
/// Histogram: proxy duration in seconds, by phase.
pub const REQUEST_DURATION_SECONDS: &str = "oagw.request.duration";
/// Gauge: requests currently inside the proxy pipeline, per upstream.
pub const REQUESTS_IN_FLIGHT: &str = "oagw.request.in_flight";
/// Counter: gateway-side faults (`5xx`) by error type.
pub const ERRORS_TOTAL: &str = "oagw.error.count";
/// Counter: gateway policy rejections (`4xx`) by error type.
pub const REJECTIONS_TOTAL: &str = "oagw.rejection.count";
/// Counter: completed WebSocket upgrades.
pub const UPGRADES_TOTAL: &str = "oagw.upgrade.count";
/// Counter: requests a rate limit refused.
pub const RATE_LIMIT_EXCEEDED_TOTAL: &str = "oagw.rate_limit.exceeded.count";
/// Gauge: share of the sustained rate budget a request consumed.
pub const RATE_LIMIT_USAGE_RATIO: &str = "oagw.rate_limit.usage_ratio";
/// Gauge: circuit-breaker state per upstream (`0` closed, `1` half-open, `2` open).
pub const CIRCUIT_BREAKER_STATE: &str = "oagw.circuit_breaker.state";

// ─── Label keys ──────────────────────────────────────────────────────

/// Label: upstream alias.
pub const LABEL_HOST: &str = "host";
/// Label: normalized request method.
pub const LABEL_METHOD: &str = "http.request.method";
/// Label: the matched route pattern, not the raw request path.
pub const LABEL_ROUTE: &str = "http.route";
/// Label: numeric response status.
pub const LABEL_STATUS: &str = "http.response.status_code";
/// Label: duration phase.
pub const LABEL_PHASE: &str = "phase";
/// Label: gateway error type.
pub const LABEL_ERROR_TYPE: &str = "error_type";
/// Label: request path, for the rate-limit instruments.
pub const LABEL_PATH: &str = "path";

/// Duration phase: the whole proxy exchange.
pub const PHASE_TOTAL: &str = "total";
/// Normalized method for anything that is not a standard verb.
pub const METHOD_OTHER: &str = "_OTHER";

/// The circuit breaker is closed and traffic flows.
pub const CIRCUIT_CLOSED: f64 = 0.0;
/// The circuit breaker is probing.
pub const CIRCUIT_HALF_OPEN: f64 = 1.0;
/// The circuit breaker is open and traffic is refused.
pub const CIRCUIT_OPEN: f64 = 2.0;

/// Circuit-breaker state, as the gauge encodes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    /// Traffic flows.
    Closed,
    /// Probing an upstream that was open.
    HalfOpen,
    /// Traffic is refused.
    Open,
}

impl CircuitState {
    /// The gauge value this state encodes.
    #[must_use]
    pub fn value(self) -> f64 {
        match self {
            Self::Closed => CIRCUIT_CLOSED,
            Self::HalfOpen => CIRCUIT_HALF_OPEN,
            Self::Open => CIRCUIT_OPEN,
        }
    }
}

/// Normalizes a method to a standard verb or `_OTHER`, per `DESIGN.md` § 4.2.
#[must_use]
pub fn normalize_method(method: &str) -> &str {
    match method {
        "GET" | "HEAD" | "POST" | "PUT" | "DELETE" | "CONNECT" | "OPTIONS" | "TRACE" | "PATCH" => {
            method
        }
        _ => METHOD_OTHER,
    }
}

/// Process-local mirrors of the counters, readable without an exporter.
#[derive(Debug, Default)]
struct LocalCounters {
    requests: AtomicU64,
    errors: AtomicU64,
    rejections: AtomicU64,
    upgrades: AtomicU64,
    rate_limit_exceeded: AtomicU64,
}

/// Counters, histograms and gauges for the proxy data plane.
pub struct ProxyMetrics {
    requests_total: Counter<u64>,
    errors_total: Counter<u64>,
    rejections_total: Counter<u64>,
    upgrades_total: Counter<u64>,
    rate_limit_exceeded_total: Counter<u64>,
    request_duration: Histogram<f64>,
    in_flight: Gauge<f64>,
    rate_limit_usage: Gauge<f64>,
    circuit_breaker_state: Gauge<f64>,
    local: LocalCounters,
    in_flight_counts: DashMap<String, AtomicI64>,
}

impl std::fmt::Debug for ProxyMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyMetrics")
            .field("requests", &self.local.requests.load(Ordering::Relaxed))
            .field("errors", &self.local.errors.load(Ordering::Relaxed))
            .field("rejections", &self.local.rejections.load(Ordering::Relaxed))
            .field("upgrades", &self.local.upgrades.load(Ordering::Relaxed))
            .field(
                "rate_limit_exceeded",
                &self.local.rate_limit_exceeded.load(Ordering::Relaxed),
            )
            .finish_non_exhaustive()
    }
}

impl Default for ProxyMetrics {
    fn default() -> Self {
        // A handle built from the global provider: the host installs it during
        // startup, so this is the production constructor.
        Self::with_meter(&global::meter(METER_NAME))
    }
}

impl ProxyMetrics {
    /// Creates the metric set from the globally installed meter provider.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Creates the metric set from an explicit meter, for tests.
    #[must_use]
    pub fn with_meter(meter: &Meter) -> Self {
        Self {
            requests_total: meter
                .u64_counter(REQUESTS_TOTAL)
                .with_description("Proxied requests")
                .with_unit("{request}")
                .build(),
            errors_total: meter
                .u64_counter(ERRORS_TOTAL)
                .with_description("Gateway-side faults")
                .with_unit("{error}")
                .build(),
            rejections_total: meter
                .u64_counter(REJECTIONS_TOTAL)
                .with_description("Gateway policy rejections")
                .with_unit("{request}")
                .build(),
            upgrades_total: meter
                .u64_counter(UPGRADES_TOTAL)
                .with_description("Completed WebSocket upgrades")
                .with_unit("{upgrade}")
                .build(),
            rate_limit_exceeded_total: meter
                .u64_counter(RATE_LIMIT_EXCEEDED_TOTAL)
                .with_description("Requests a rate limit refused")
                .with_unit("{request}")
                .build(),
            request_duration: meter
                .f64_histogram(REQUEST_DURATION_SECONDS)
                .with_description("Proxy exchange duration")
                .with_unit("s")
                .build(),
            in_flight: meter
                .f64_gauge(REQUESTS_IN_FLIGHT)
                .with_description("Requests currently inside the pipeline")
                .with_unit("{request}")
                .build(),
            rate_limit_usage: meter
                .f64_gauge(RATE_LIMIT_USAGE_RATIO)
                .with_description("Share of the sustained rate budget consumed")
                .with_unit("1")
                .build(),
            circuit_breaker_state: meter
                .f64_gauge(CIRCUIT_BREAKER_STATE)
                .with_description("Circuit-breaker state per upstream")
                .with_unit("1")
                .build(),
            local: LocalCounters::default(),
            in_flight_counts: DashMap::new(),
        }
    }

    /// Records a proxied request with its upstream status.
    pub fn record_request(&self, host: &str, method: &str, route: &str, status: u16) {
        self.local.requests.fetch_add(1, Ordering::Relaxed);
        self.requests_total.add(
            1,
            &[
                KeyValue::new(LABEL_HOST, host.to_owned()),
                KeyValue::new(LABEL_METHOD, normalize_method(method).to_owned()),
                KeyValue::new(LABEL_ROUTE, route.to_owned()),
                KeyValue::new(LABEL_STATUS, status.to_string()),
            ],
        );
    }

    /// Records a gateway-side fault (`5xx`).
    pub fn record_error(&self, host: &str, error_type: &str) {
        self.local.errors.fetch_add(1, Ordering::Relaxed);
        self.errors_total.add(
            1,
            &[
                KeyValue::new(LABEL_HOST, host.to_owned()),
                KeyValue::new(LABEL_ERROR_TYPE, error_type.to_owned()),
            ],
        );
    }

    /// Records a gateway policy rejection (`4xx`).
    pub fn record_rejection(&self, host: &str, error_type: &str) {
        self.local.rejections.fetch_add(1, Ordering::Relaxed);
        self.rejections_total.add(
            1,
            &[
                KeyValue::new(LABEL_HOST, host.to_owned()),
                KeyValue::new(LABEL_ERROR_TYPE, error_type.to_owned()),
            ],
        );
    }

    /// Records a completed WebSocket splice.
    pub fn record_upgrade(&self, host: &str, route: &str) {
        self.local.upgrades.fetch_add(1, Ordering::Relaxed);
        self.upgrades_total.add(
            1,
            &[
                KeyValue::new(LABEL_HOST, host.to_owned()),
                KeyValue::new(LABEL_ROUTE, route.to_owned()),
            ],
        );
    }

    /// Records a request a rate limit refused.
    pub fn record_rate_limit_exceeded(&self, host: &str, path: &str) {
        self.local
            .rate_limit_exceeded
            .fetch_add(1, Ordering::Relaxed);
        self.rate_limit_exceeded_total.add(
            1,
            &[
                KeyValue::new(LABEL_HOST, host.to_owned()),
                KeyValue::new(LABEL_PATH, path.to_owned()),
            ],
        );
    }

    /// Observes the duration of one proxy exchange.
    pub fn observe_request_duration(
        &self,
        host: &str,
        method: &str,
        route: &str,
        phase: &str,
        seconds: f64,
    ) {
        self.request_duration.record(
            seconds,
            &[
                KeyValue::new(LABEL_HOST, host.to_owned()),
                KeyValue::new(LABEL_METHOD, normalize_method(method).to_owned()),
                KeyValue::new(LABEL_ROUTE, route.to_owned()),
                KeyValue::new(LABEL_PHASE, phase.to_owned()),
            ],
        );
    }

    /// Observes the share of the sustained rate budget one request consumed,
    /// clamped to `0.0..=1.0`.
    pub fn observe_rate_limit_usage(&self, host: &str, path: &str, consumed: f64) {
        self.rate_limit_usage.record(
            consumed.clamp(0.0, 1.0),
            &[
                KeyValue::new(LABEL_HOST, host.to_owned()),
                KeyValue::new(LABEL_PATH, path.to_owned()),
            ],
        );
    }

    /// Records the circuit-breaker state of an upstream.
    pub fn set_circuit_breaker_state(&self, host: &str, state: CircuitState) {
        self.circuit_breaker_state
            .record(state.value(), &[KeyValue::new(LABEL_HOST, host.to_owned())]);
    }

    /// Marks one request as in flight for the addressed alias.
    ///
    /// The returned guard clears it again on drop, so an aborted request does
    /// not leave the gauge stuck.
    #[must_use]
    pub fn enter(self: &Arc<Self>, host: &str) -> InFlightGuard {
        let count = self
            .in_flight_counts
            .entry(host.to_owned())
            .or_default()
            .fetch_add(1, Ordering::Relaxed)
            + 1;
        self.in_flight
            .record(count as f64, &[KeyValue::new(LABEL_HOST, host.to_owned())]);
        InFlightGuard {
            host: host.to_owned(),
            metrics: Arc::clone(self),
        }
    }

    fn leave(&self, host: &str) {
        if let Some(count) = self
            .in_flight_counts
            .get(host)
            .map(|entry| entry.fetch_sub(1, Ordering::Relaxed))
        {
            self.in_flight.record(
                (count - 1) as f64,
                &[KeyValue::new(LABEL_HOST, host.to_owned())],
            );
        }
    }

    /// Total proxied requests.
    #[must_use]
    pub fn requests_total(&self) -> u64 {
        self.local.requests.load(Ordering::Relaxed)
    }

    /// Total gateway-side faults.
    #[must_use]
    pub fn errors_total(&self) -> u64 {
        self.local.errors.load(Ordering::Relaxed)
    }

    /// Total policy rejections.
    #[must_use]
    pub fn rejections_total(&self) -> u64 {
        self.local.rejections.load(Ordering::Relaxed)
    }

    /// Total completed upgrades.
    #[must_use]
    pub fn upgrades_total(&self) -> u64 {
        self.local.upgrades.load(Ordering::Relaxed)
    }

    /// Total rate-limit refusals.
    #[must_use]
    pub fn rate_limit_exceeded_total(&self) -> u64 {
        self.local.rate_limit_exceeded.load(Ordering::Relaxed)
    }
}

/// Clears the in-flight gauge for one request when it leaves the pipeline.
pub struct InFlightGuard {
    host: String,
    metrics: Arc<ProxyMetrics>,
}

impl std::fmt::Debug for InFlightGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InFlightGuard")
            .field("host", &self.host)
            .finish()
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.metrics.leave(&self.host);
    }
}

/// The `metrics` catalog identifier, exposed for the catalog tests.
#[must_use]
pub fn metrics_plugin_id() -> &'static str {
    crate::domain::gts_helpers::RESERVED_METRICS_TRANSFORM_PLUGIN_ID
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::metrics::MeterProvider;
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
    use opentelemetry_sdk::metrics::{
        InMemoryMetricExporter, Instrument, PeriodicReader, SdkMeterProvider, Stream,
    };

    /// An in-memory provider, so the instruments can be read back.
    struct Harness {
        provider: SdkMeterProvider,
        exporter: InMemoryMetricExporter,
    }

    impl Harness {
        fn new() -> Self {
            let exporter = InMemoryMetricExporter::default();
            let provider = SdkMeterProvider::builder()
                .with_reader(PeriodicReader::builder(exporter.clone()).build())
                .with_view(|_: &Instrument| Stream::builder().build().ok())
                .build();
            Self { provider, exporter }
        }

        fn metrics(&self) -> Arc<ProxyMetrics> {
            Arc::new(ProxyMetrics::with_meter(&self.provider.meter(METER_NAME)))
        }

        fn flush(&self) {
            self.provider.force_flush().expect("flush");
        }

        fn values(&self, name: &str, attrs: &[(&str, &str)]) -> Vec<f64> {
            self.exporter
                .get_finished_metrics()
                .expect("exporter readable")
                .iter()
                .flat_map(|resource| resource.scope_metrics())
                .flat_map(|scope| scope.metrics())
                .filter(|metric| metric.name() == name)
                .flat_map(|metric| match metric.data() {
                    AggregatedMetrics::U64(MetricData::Sum(sum)) => sum
                        .data_points()
                        .filter(|point| matches(point.attributes(), attrs))
                        .map(|point| point.value() as f64)
                        .collect::<Vec<_>>(),
                    AggregatedMetrics::F64(MetricData::Sum(sum)) => sum
                        .data_points()
                        .filter(|point| matches(point.attributes(), attrs))
                        .map(|point| point.value())
                        .collect::<Vec<_>>(),
                    AggregatedMetrics::F64(MetricData::Gauge(gauge)) => gauge
                        .data_points()
                        .filter(|point| matches(point.attributes(), attrs))
                        .map(|point| point.value())
                        .collect::<Vec<_>>(),
                    AggregatedMetrics::F64(MetricData::Histogram(histogram)) => histogram
                        .data_points()
                        .filter(|point| matches(point.attributes(), attrs))
                        .map(|point| point.count() as f64)
                        .collect::<Vec<_>>(),
                    _ => Vec::new(),
                })
                .collect()
        }
    }

    fn matches<'a>(
        attributes: impl Iterator<Item = &'a KeyValue>,
        expected: &[(&str, &str)],
    ) -> bool {
        let found: Vec<(String, String)> = attributes
            .map(|pair| {
                (
                    pair.key.as_str().to_owned(),
                    pair.value.as_str().to_string(),
                )
            })
            .collect();
        expected
            .iter()
            .all(|(key, value)| found.iter().any(|(name, val)| name == key && val == value))
    }

    #[test]
    fn counters_start_at_zero_and_increment() {
        let metrics = ProxyMetrics::with_meter(&global::meter(METER_NAME));
        assert_eq!(metrics.requests_total(), 0);
        metrics.record_request("api.example.com", "GET", "/v1", 200);
        metrics.record_request("api.example.com", "PATCH", "/v1", 201);
        metrics.record_error("api.example.com", "cf.oagw.upstream.unavailable.v1");
        metrics.record_rejection("api.example.com", "cf.oagw.route.not_found.v1");
        metrics.record_upgrade("api.example.com", "/v1/ws");
        metrics.record_rate_limit_exceeded("api.example.com", "/v1");
        assert_eq!(metrics.requests_total(), 2);
        assert_eq!(metrics.errors_total(), 1);
        assert_eq!(metrics.rejections_total(), 1);
        assert_eq!(metrics.upgrades_total(), 1);
        assert_eq!(metrics.rate_limit_exceeded_total(), 1);
    }

    #[test]
    fn methods_outside_the_standard_verbs_are_normalized() {
        assert_eq!(normalize_method("GET"), "GET");
        assert_eq!(normalize_method("PROPFIND"), METHOD_OTHER);
    }

    #[test]
    fn every_instrument_is_reported_through_the_provider() {
        let harness = Harness::new();
        let metrics = harness.metrics();
        let guard = metrics.enter("api.example.com");
        metrics.record_request("api.example.com", "GET", "/v1/chat", 200);
        metrics.observe_request_duration("api.example.com", "GET", "/v1/chat", PHASE_TOTAL, 0.004);
        metrics.record_error("api.example.com", "cf.oagw.upstream.bad_gateway.v1");
        metrics.record_rejection("api.example.com", "cf.oagw.route.not_found.v1");
        metrics.record_upgrade("api.example.com", "/v1/ws");
        metrics.record_rate_limit_exceeded("api.example.com", "/v1/chat");
        metrics.observe_rate_limit_usage("api.example.com", "/v1/chat", 0.5);
        metrics.set_circuit_breaker_state("api.example.com", CircuitState::Closed);
        drop(guard);
        harness.flush();

        let host = [("host", "api.example.com")];
        assert_eq!(
            harness.values(REQUESTS_TOTAL, &host).iter().sum::<f64>(),
            1.0,
            "requests counter"
        );
        assert_eq!(
            harness.values(ERRORS_TOTAL, &host).iter().sum::<f64>(),
            1.0,
            "errors counter"
        );
        assert_eq!(
            harness.values(REJECTIONS_TOTAL, &host).iter().sum::<f64>(),
            1.0,
            "rejections counter"
        );
        assert_eq!(
            harness.values(UPGRADES_TOTAL, &host).iter().sum::<f64>(),
            1.0,
            "upgrades counter"
        );
        assert_eq!(
            harness
                .values(RATE_LIMIT_EXCEEDED_TOTAL, &host)
                .iter()
                .sum::<f64>(),
            1.0,
            "rate-limit refusals"
        );
        assert_eq!(
            harness.values(REQUEST_DURATION_SECONDS, &host).len(),
            1,
            "duration histogram"
        );
        assert_eq!(
            harness.values(REQUESTS_IN_FLIGHT, &host).last().copied(),
            Some(0.0),
            "in-flight gauge returns to zero when the guard drops"
        );
        assert_eq!(
            harness
                .values(RATE_LIMIT_USAGE_RATIO, &host)
                .last()
                .copied(),
            Some(0.5),
            "rate-limit usage gauge"
        );
        assert_eq!(
            harness.values(CIRCUIT_BREAKER_STATE, &host).last().copied(),
            Some(CIRCUIT_CLOSED),
            "circuit-breaker gauge"
        );
    }

    #[test]
    fn the_in_flight_gauge_counts_concurrent_requests_per_host() {
        let harness = Harness::new();
        let metrics = harness.metrics();
        let first = metrics.enter("a.example.com");
        let _second = metrics.enter("a.example.com");
        let other = metrics.enter("b.example.com");
        drop(first);
        harness.flush();
        let a = [("host", "a.example.com")];
        let b = [("host", "b.example.com")];
        assert_eq!(
            harness.values(REQUESTS_IN_FLIGHT, &a).last().copied(),
            Some(1.0)
        );
        assert_eq!(
            harness.values(REQUESTS_IN_FLIGHT, &b).last().copied(),
            Some(1.0)
        );
        drop(_second);
        drop(other);
        harness.flush();
        assert_eq!(
            harness.values(REQUESTS_IN_FLIGHT, &a).last().copied(),
            Some(0.0)
        );
    }

    #[test]
    fn the_circuit_states_encode_the_documented_values() {
        assert_eq!(CircuitState::Closed.value(), CIRCUIT_CLOSED);
        assert_eq!(CircuitState::HalfOpen.value(), CIRCUIT_HALF_OPEN);
        assert_eq!(CircuitState::Open.value(), CIRCUIT_OPEN);
    }

    #[test]
    fn the_metrics_identifier_is_the_catalog_one() {
        assert_eq!(metrics_plugin_id(), "cf.core.oagw.metrics.v1");
    }
}
