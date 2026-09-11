//! OpenTelemetry instrumentation for the proxy hot path
//! (`cpt-cf-oagw-nfr-observability`).
//!
//! Instrument names are literal Prometheus names (counters end in `_total`,
//! duration histograms in `_seconds`), matching the platform's
//! `add_metric_suffixes: false` collector posture. Label keys follow the OTel
//! HTTP semantic conventions so the inbound and outbound gateways share
//! dashboards; `host` is the OAGW-specific label carrying the upstream alias.
//!
//! Cardinality: no tenant labels, `http.route` is the matched route pattern
//! rather than the raw path, and the method is normalized to a standard verb
//! or `_OTHER`.

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Gauge, Histogram, Meter};

/// Meter / instrumentation scope name.
pub const METER_NAME: &str = "oagw";

const REQUESTS_TOTAL: &str = "oagw_requests_total";
const REQUEST_DURATION: &str = "oagw_request_duration_seconds";
const REQUESTS_IN_FLIGHT: &str = "oagw_requests_in_flight";
const ERRORS_TOTAL: &str = "oagw_errors_total";
const CIRCUIT_BREAKER_STATE: &str = "oagw_circuit_breaker_state";
const CIRCUIT_BREAKER_TRANSITIONS: &str = "oagw_circuit_breaker_transitions_total";
const RATE_LIMIT_EXCEEDED_TOTAL: &str = "oagw_rate_limit_exceeded_total";
const RATE_LIMIT_USAGE_RATIO: &str = "oagw_rate_limit_usage_ratio";
const ROUTING_TARGET_HOST_USED: &str = "oagw_routing_target_host_used";
const ROUTING_ENDPOINT_SELECTED: &str = "oagw_routing_endpoint_selected";
const UPSTREAM_AVAILABLE: &str = "oagw_upstream_available";

/// Duration histogram buckets, per DESIGN §4.2.
pub const DURATION_BUCKETS: &[f64] = &[
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Methods that keep their own label value; anything else is `_OTHER`.
const KNOWN_METHODS: &[&str] = &[
    "GET", "HEAD", "POST", "PUT", "DELETE", "CONNECT", "OPTIONS", "TRACE", "PATCH",
];

/// Normalize an HTTP method for use as a metric label.
#[must_use]
pub fn normalize_method(method: &str) -> &'static str {
    KNOWN_METHODS
        .iter()
        .find(|known| method.eq_ignore_ascii_case(known))
        .copied()
        .unwrap_or("_OTHER")
}

/// Proxy metrics handle.
pub struct OagwMetrics {
    requests: Counter<u64>,
    duration: Histogram<f64>,
    in_flight: Gauge<i64>,
    errors: Counter<u64>,
    circuit_breaker_state: Gauge<i64>,
    circuit_breaker_transitions: Counter<u64>,
    rate_limit_exceeded: Counter<u64>,
    rate_limit_usage: Gauge<f64>,
    target_host_used: Counter<u64>,
    endpoint_selected: Counter<u64>,
    upstream_available: Gauge<i64>,
    live_requests: std::sync::atomic::AtomicI64,
}

impl std::fmt::Debug for OagwMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OagwMetrics").finish_non_exhaustive()
    }
}

impl OagwMetrics {
    /// Build the instrument set from `meter`.
    #[must_use]
    pub fn new(meter: &Meter) -> Self {
        Self {
            requests: meter
                .u64_counter(REQUESTS_TOTAL)
                .with_description("Proxied requests by upstream, method, route and status")
                .build(),
            duration: meter
                .f64_histogram(REQUEST_DURATION)
                .with_description("Proxy request duration in seconds, by phase")
                .with_boundaries(DURATION_BUCKETS.to_vec())
                .build(),
            in_flight: meter
                .i64_gauge(REQUESTS_IN_FLIGHT)
                .with_description("Proxy requests currently in flight")
                .build(),
            errors: meter
                .u64_counter(ERRORS_TOTAL)
                .with_description("Gateway-originated proxy errors by type")
                .build(),
            circuit_breaker_state: meter
                .i64_gauge(CIRCUIT_BREAKER_STATE)
                .with_description("Circuit breaker state (0=closed, 1=half-open, 2=open)")
                .build(),
            circuit_breaker_transitions: meter
                .u64_counter(CIRCUIT_BREAKER_TRANSITIONS)
                .with_description("Circuit breaker state transitions")
                .build(),
            rate_limit_exceeded: meter
                .u64_counter(RATE_LIMIT_EXCEEDED_TOTAL)
                .with_description("Requests refused or degraded by a rate limit")
                .build(),
            rate_limit_usage: meter
                .f64_gauge(RATE_LIMIT_USAGE_RATIO)
                .with_description("Fraction of the rate limit budget consumed (0.0 - 1.0)")
                .build(),
            target_host_used: meter
                .u64_counter(ROUTING_TARGET_HOST_USED)
                .with_description("Requests routed via an explicit X-OAGW-Target-Host header")
                .build(),
            endpoint_selected: meter
                .u64_counter(ROUTING_ENDPOINT_SELECTED)
                .with_description("Endpoint selections by method")
                .build(),
            upstream_available: meter
                .i64_gauge(UPSTREAM_AVAILABLE)
                .with_description("Whether an upstream endpoint is reachable (0=down, 1=up)")
                .build(),
            live_requests: std::sync::atomic::AtomicI64::new(0),
        }
    }

    /// Build from the process-global meter provider.
    #[must_use]
    pub fn from_global() -> Self {
        Self::new(&opentelemetry::global::meter(METER_NAME))
    }

    /// Record a completed proxy request.
    pub fn record_request(&self, host: &str, method: &str, route: &str, status: u16) {
        self.requests.add(
            1,
            &[
                KeyValue::new("host", host.to_owned()),
                KeyValue::new("http.request.method", normalize_method(method)),
                KeyValue::new("http.route", route.to_owned()),
                KeyValue::new("http.response.status_code", i64::from(status)),
            ],
        );
    }

    /// Record a phase duration.
    pub fn record_duration(&self, host: &str, route: &str, phase: &'static str, seconds: f64) {
        self.duration.record(
            seconds,
            &[
                KeyValue::new("host", host.to_owned()),
                KeyValue::new("http.route", route.to_owned()),
                KeyValue::new("phase", phase),
            ],
        );
    }

    /// Note that a request entered the proxy path.
    pub fn request_started(&self, host: &str) {
        let live = self
            .live_requests
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        self.in_flight
            .record(live, &[KeyValue::new("host", host.to_owned())]);
    }

    /// Note that a request left the proxy path.
    pub fn request_finished(&self, host: &str) {
        let live = self
            .live_requests
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed)
            - 1;
        self.in_flight
            .record(live.max(0), &[KeyValue::new("host", host.to_owned())]);
    }

    /// Record a gateway-originated error.
    pub fn record_error(&self, host: &str, route: &str, error_type: &str) {
        self.errors.add(
            1,
            &[
                KeyValue::new("host", host.to_owned()),
                KeyValue::new("http.route", route.to_owned()),
                KeyValue::new("error_type", error_type.to_owned()),
            ],
        );
    }

    /// Record a rate limit refusal or degradation.
    pub fn record_rate_limit_exceeded(&self, host: &str, path: &str) {
        self.rate_limit_exceeded.add(
            1,
            &[
                KeyValue::new("host", host.to_owned()),
                KeyValue::new("path", path.to_owned()),
            ],
        );
    }

    /// Record how much of the rate limit budget is consumed.
    pub fn record_rate_limit_usage(&self, host: &str, path: &str, ratio: f64) {
        self.rate_limit_usage.record(
            ratio.clamp(0.0, 1.0),
            &[
                KeyValue::new("host", host.to_owned()),
                KeyValue::new("path", path.to_owned()),
            ],
        );
    }

    /// Record endpoint selection.
    pub fn record_endpoint_selection(
        &self,
        upstream_id: &str,
        endpoint_host: &str,
        selection_method: &'static str,
    ) {
        self.endpoint_selected.add(
            1,
            &[
                KeyValue::new("upstream_id", upstream_id.to_owned()),
                KeyValue::new("endpoint_host", endpoint_host.to_owned()),
                KeyValue::new("selection_method", selection_method),
            ],
        );
        if selection_method == "explicit_header" {
            self.target_host_used.add(
                1,
                &[
                    KeyValue::new("upstream_id", upstream_id.to_owned()),
                    KeyValue::new("endpoint_host", endpoint_host.to_owned()),
                ],
            );
        }
    }

    /// Record whether an endpoint answered.
    pub fn record_upstream_available(&self, host: &str, endpoint: &str, available: bool) {
        self.upstream_available.record(
            i64::from(available),
            &[
                KeyValue::new("host", host.to_owned()),
                KeyValue::new("endpoint", endpoint.to_owned()),
            ],
        );
    }

    /// Record a circuit breaker transition and its resulting state.
    pub fn record_circuit_breaker(&self, host: &str, from_state: &str, to_state: &str, state: i64) {
        self.circuit_breaker_transitions.add(
            1,
            &[
                KeyValue::new("host", host.to_owned()),
                KeyValue::new("from_state", from_state.to_owned()),
                KeyValue::new("to_state", to_state.to_owned()),
            ],
        );
        self.circuit_breaker_state
            .record(state, &[KeyValue::new("host", host.to_owned())]);
    }
}

#[cfg(test)]
mod tests {
    use super::{DURATION_BUCKETS, OagwMetrics, normalize_method};

    #[test]
    fn method_normalization_bounds_cardinality() {
        assert_eq!(normalize_method("get"), "GET");
        assert_eq!(normalize_method("POST"), "POST");
        assert_eq!(normalize_method("PROPFIND"), "_OTHER");
        assert_eq!(normalize_method(""), "_OTHER");
    }

    #[test]
    fn duration_buckets_match_the_design() {
        assert_eq!(DURATION_BUCKETS.first(), Some(&0.001));
        assert_eq!(DURATION_BUCKETS.last(), Some(&10.0));
        assert_eq!(DURATION_BUCKETS.len(), 12);
        assert!(
            DURATION_BUCKETS.windows(2).all(|w| w[0] < w[1]),
            "buckets must be strictly increasing"
        );
    }

    #[test]
    fn recording_against_the_noop_provider_is_safe() {
        // No exporter is installed in tests; every instrument must still be
        // usable without panicking.
        let metrics = OagwMetrics::from_global();
        metrics.request_started("api.openai.com");
        metrics.record_request("api.openai.com", "post", "/v1/chat", 200);
        metrics.record_duration("api.openai.com", "/v1/chat", "upstream", 0.01);
        metrics.record_error("api.openai.com", "/v1/chat", "downstream_error");
        metrics.record_rate_limit_exceeded("api.openai.com", "/v1/chat");
        metrics.record_rate_limit_usage("api.openai.com", "/v1/chat", 2.0);
        metrics.record_endpoint_selection("u1", "api.openai.com", "explicit_header");
        metrics.record_upstream_available("api.openai.com", "api.openai.com", true);
        metrics.record_circuit_breaker("api.openai.com", "closed", "open", 2);
        metrics.request_finished("api.openai.com");
    }
}
