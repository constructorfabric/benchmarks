//! Prometheus/OTel instrumentation for the proxy path.
//!
//! Instrument names are the literal Prometheus names from `DESIGN.md` § 4.2
//! (counters end in `_total`, duration histograms in `_seconds`) and the label
//! keys follow the OTel HTTP semantic conventions so OAGW and the inbound API
//! Gateway can share dashboards. Cardinality is bounded deliberately: no
//! tenant label, `http.route` is the matched route pattern rather than the raw
//! path, and the method is normalised to a standard verb or `_OTHER`.

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Gauge, Histogram, Meter};

/// Instrumentation scope name.
pub const METER_NAME: &str = "oagw";

const REQUESTS_TOTAL: &str = "oagw_requests_total";
const REQUEST_DURATION: &str = "oagw_request_duration_seconds";
const REQUESTS_IN_FLIGHT: &str = "oagw_requests_in_flight";
const ERRORS_TOTAL: &str = "oagw_errors_total";
const RATE_LIMIT_EXCEEDED_TOTAL: &str = "oagw_rate_limit_exceeded_total";
const RATE_LIMIT_USAGE_RATIO: &str = "oagw_rate_limit_usage_ratio";
const ROUTING_TARGET_HOST_USED: &str = "oagw_routing_target_host_used";
const ROUTING_ENDPOINT_SELECTED: &str = "oagw_routing_endpoint_selected";
const UPSTREAM_AVAILABLE: &str = "oagw_upstream_available";

/// Histogram buckets for request duration, in seconds.
pub const DURATION_BUCKETS: &[f64] = &[
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Standard HTTP verbs; anything else is reported as `_OTHER`.
const STANDARD_METHODS: &[&str] = &[
    "GET", "HEAD", "POST", "PUT", "DELETE", "CONNECT", "OPTIONS", "TRACE", "PATCH",
];

/// Normalise an HTTP method for use as a metric label.
#[must_use]
pub fn normalize_method(method: &str) -> &'static str {
    let upper = method.to_ascii_uppercase();
    STANDARD_METHODS
        .iter()
        .find(|standard| **standard == upper.as_str())
        .copied()
        .unwrap_or("_OTHER")
}

/// How an endpoint was chosen from a pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMethod {
    /// `X-OAGW-Target-Host` named it.
    ExplicitHeader,
    /// Round-robin across the pool.
    RoundRobin,
    /// The pool has exactly one member.
    Default,
}

impl SelectionMethod {
    /// Metric label value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExplicitHeader => "explicit_header",
            Self::RoundRobin => "round_robin",
            Self::Default => "default",
        }
    }
}

/// OAGW's metric handles.
pub struct OagwMetrics {
    requests_total: Counter<u64>,
    request_duration: Histogram<f64>,
    requests_in_flight: Gauge<i64>,
    errors_total: Counter<u64>,
    rate_limit_exceeded_total: Counter<u64>,
    rate_limit_usage_ratio: Gauge<f64>,
    routing_target_host_used: Counter<u64>,
    routing_endpoint_selected: Counter<u64>,
    upstream_available: Gauge<i64>,
}

impl std::fmt::Debug for OagwMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OagwMetrics")
    }
}

impl OagwMetrics {
    /// Build the instruments from a meter.
    #[must_use]
    pub fn new(meter: &Meter) -> Self {
        Self {
            requests_total: meter.u64_counter(REQUESTS_TOTAL).build(),
            request_duration: meter
                .f64_histogram(REQUEST_DURATION)
                .with_boundaries(DURATION_BUCKETS.to_vec())
                .build(),
            requests_in_flight: meter.i64_gauge(REQUESTS_IN_FLIGHT).build(),
            errors_total: meter.u64_counter(ERRORS_TOTAL).build(),
            rate_limit_exceeded_total: meter.u64_counter(RATE_LIMIT_EXCEEDED_TOTAL).build(),
            rate_limit_usage_ratio: meter.f64_gauge(RATE_LIMIT_USAGE_RATIO).build(),
            routing_target_host_used: meter.u64_counter(ROUTING_TARGET_HOST_USED).build(),
            routing_endpoint_selected: meter.u64_counter(ROUTING_ENDPOINT_SELECTED).build(),
            upstream_available: meter.i64_gauge(UPSTREAM_AVAILABLE).build(),
        }
    }

    /// Build the instruments from the process-global meter provider.
    #[must_use]
    pub fn from_global() -> Self {
        Self::new(&opentelemetry::global::meter(METER_NAME))
    }

    /// Record a completed proxy request.
    pub fn record_request(&self, host: &str, method: &str, route: &str, status: u16, secs: f64) {
        let labels = [
            KeyValue::new("host", host.to_owned()),
            KeyValue::new("http.request.method", normalize_method(method)),
            KeyValue::new("http.route", route.to_owned()),
            KeyValue::new("http.response.status_code", i64::from(status)),
        ];
        self.requests_total.add(1, &labels);
        self.request_duration.record(
            secs,
            &[
                KeyValue::new("host", host.to_owned()),
                KeyValue::new("http.route", route.to_owned()),
                KeyValue::new("phase", "total"),
            ],
        );
    }

    /// Record time spent in one phase of the pipeline.
    pub fn record_phase(&self, host: &str, route: &str, phase: &'static str, secs: f64) {
        self.request_duration.record(
            secs,
            &[
                KeyValue::new("host", host.to_owned()),
                KeyValue::new("http.route", route.to_owned()),
                KeyValue::new("phase", phase),
            ],
        );
    }

    /// Report the current in-flight count for a host.
    pub fn set_in_flight(&self, host: &str, value: i64) {
        self.requests_in_flight
            .record(value, &[KeyValue::new("host", host.to_owned())]);
    }

    /// Record a gateway error by type.
    pub fn record_error(&self, host: &str, route: &str, error_type: &str) {
        self.errors_total.add(
            1,
            &[
                KeyValue::new("host", host.to_owned()),
                KeyValue::new("http.route", route.to_owned()),
                KeyValue::new("error_type", error_type.to_owned()),
            ],
        );
    }

    /// Record a rate-limit rejection.
    pub fn record_rate_limited(&self, host: &str, path: &str) {
        self.rate_limit_exceeded_total.add(
            1,
            &[
                KeyValue::new("host", host.to_owned()),
                KeyValue::new("path", path.to_owned()),
            ],
        );
    }

    /// Report bucket usage as a ratio in `[0.0, 1.0]`.
    pub fn set_rate_limit_usage(&self, host: &str, path: &str, ratio: f64) {
        self.rate_limit_usage_ratio.record(
            ratio.clamp(0.0, 1.0),
            &[
                KeyValue::new("host", host.to_owned()),
                KeyValue::new("path", path.to_owned()),
            ],
        );
    }

    /// Record which endpoint was selected, and how.
    pub fn record_endpoint_selected(
        &self,
        upstream_id: &str,
        endpoint_host: &str,
        selection: SelectionMethod,
    ) {
        self.routing_endpoint_selected.add(
            1,
            &[
                KeyValue::new("upstream_id", upstream_id.to_owned()),
                KeyValue::new("endpoint_host", endpoint_host.to_owned()),
                KeyValue::new("selection_method", selection.as_str()),
            ],
        );
        if selection == SelectionMethod::ExplicitHeader {
            self.routing_target_host_used.add(
                1,
                &[
                    KeyValue::new("upstream_id", upstream_id.to_owned()),
                    KeyValue::new("endpoint_host", endpoint_host.to_owned()),
                ],
            );
        }
    }

    /// Report upstream reachability (`0` down, `1` up).
    pub fn set_upstream_available(&self, host: &str, endpoint: &str, available: bool) {
        self.upstream_available.record(
            i64::from(available),
            &[
                KeyValue::new("host", host.to_owned()),
                KeyValue::new("endpoint", endpoint.to_owned()),
            ],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn methods_are_normalized_to_a_bounded_set() {
        assert_eq!(normalize_method("get"), "GET");
        assert_eq!(normalize_method("POST"), "POST");
        assert_eq!(normalize_method("PROPFIND"), "_OTHER");
        assert_eq!(normalize_method(""), "_OTHER");
    }

    #[test]
    fn selection_labels_match_the_design() {
        assert_eq!(SelectionMethod::ExplicitHeader.as_str(), "explicit_header");
        assert_eq!(SelectionMethod::RoundRobin.as_str(), "round_robin");
        assert_eq!(SelectionMethod::Default.as_str(), "default");
    }

    #[test]
    fn instruments_are_constructible_without_an_exporter() {
        // A no-op meter provider is the default until the host installs one;
        // recording against it must not panic.
        let metrics = OagwMetrics::from_global();
        metrics.record_request("api.example.com", "get", "/v1/chat", 200, 0.01);
        metrics.record_error("api.example.com", "/v1/chat", "downstream_error");
        metrics.record_rate_limited("api.example.com", "/v1/chat");
        metrics.set_rate_limit_usage("api.example.com", "/v1/chat", 2.0);
        metrics.set_in_flight("api.example.com", 3);
        metrics.record_endpoint_selected("u", "a.example.com", SelectionMethod::RoundRobin);
        metrics.set_upstream_available("api.example.com", "a.example.com", true);
        metrics.record_phase("api.example.com", "/v1/chat", "upstream", 0.5);
    }

    #[test]
    fn duration_buckets_match_the_design() {
        assert_eq!(DURATION_BUCKETS.first(), Some(&0.001));
        assert_eq!(DURATION_BUCKETS.last(), Some(&10.0));
        assert_eq!(DURATION_BUCKETS.len(), 12);
    }
}
