//! Data Plane instrumentation.
//!
//! The metric names and label keys are `docs/DESIGN.md` §4.2. Cardinality is
//! deliberately bounded: no tenant labels, `http.route` is the matched route
//! pattern rather than the raw path, and the method is normalized to a
//! standard verb or `_OTHER` — the same vocabulary the inbound API Gateway
//! uses, so both gateways share dashboards.

use opentelemetry::metrics::{Counter, Histogram, Meter, UpDownCounter};
use opentelemetry::{KeyValue, global};

/// Histogram buckets for request duration, in seconds.
pub const DURATION_BUCKETS: [f64; 12] = [
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Normalize an HTTP method to a standard verb or `_OTHER`, per the OTel HTTP
/// semantic conventions.
#[must_use]
pub fn normalize_method(method: &str) -> &'static str {
    match method.to_ascii_uppercase().as_str() {
        "GET" => "GET",
        "HEAD" => "HEAD",
        "POST" => "POST",
        "PUT" => "PUT",
        "DELETE" => "DELETE",
        "CONNECT" => "CONNECT",
        "OPTIONS" => "OPTIONS",
        "TRACE" => "TRACE",
        "PATCH" => "PATCH",
        _ => "_OTHER",
    }
}

/// OAGW's metric handles.
pub struct OagwMetrics {
    requests_total: Counter<u64>,
    request_duration: Histogram<f64>,
    requests_in_flight: UpDownCounter<i64>,
    errors_total: Counter<u64>,
    rate_limit_exceeded_total: Counter<u64>,
    routing_target_host_used: Counter<u64>,
    routing_endpoint_selected: Counter<u64>,
}

impl std::fmt::Debug for OagwMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OagwMetrics").finish_non_exhaustive()
    }
}

impl OagwMetrics {
    /// Build the instruments from the global meter provider.
    #[must_use]
    pub fn from_global() -> Self {
        Self::new(&global::meter("oagw"))
    }

    /// Build the instruments from a specific meter (used by tests).
    #[must_use]
    pub fn new(meter: &Meter) -> Self {
        Self {
            requests_total: meter
                .u64_counter("oagw_requests_total")
                .with_description("Proxy requests by upstream, method, route and status")
                .build(),
            request_duration: meter
                .f64_histogram("oagw_request_duration_seconds")
                .with_description("Proxy request duration by phase")
                .with_boundaries(DURATION_BUCKETS.to_vec())
                .build(),
            requests_in_flight: meter
                .i64_up_down_counter("oagw_requests_in_flight")
                .with_description("Proxy requests currently in flight")
                .build(),
            errors_total: meter
                .u64_counter("oagw_errors_total")
                .with_description("Gateway errors by upstream, route and error type")
                .build(),
            rate_limit_exceeded_total: meter
                .u64_counter("oagw_rate_limit_exceeded_total")
                .with_description("Requests rejected by a rate limit")
                .build(),
            routing_target_host_used: meter
                .u64_counter("oagw_routing_target_host_used")
                .with_description("Requests pinned by X-OAGW-Target-Host")
                .build(),
            routing_endpoint_selected: meter
                .u64_counter("oagw_routing_endpoint_selected")
                .with_description("Endpoint selections by method")
                .build(),
        }
    }

    /// Record a completed exchange.
    pub fn record_request(&self, host: &str, method: &str, route: &str, status: u16) {
        self.requests_total.add(
            1,
            &[
                KeyValue::new("host", host.to_owned()),
                KeyValue::new("http.request.method", normalize_method(method)),
                KeyValue::new("http.route", route.to_owned()),
                KeyValue::new("http.response.status_code", i64::from(status)),
            ],
        );
    }

    /// Record how long a phase took.
    pub fn record_duration(&self, host: &str, route: &str, phase: &'static str, seconds: f64) {
        self.request_duration.record(
            seconds,
            &[
                KeyValue::new("host", host.to_owned()),
                KeyValue::new("http.route", route.to_owned()),
                KeyValue::new("phase", phase),
            ],
        );
    }

    /// Move the in-flight gauge.
    pub fn adjust_in_flight(&self, host: &str, delta: i64) {
        self.requests_in_flight
            .add(delta, &[KeyValue::new("host", host.to_owned())]);
    }

    /// Record a gateway error.
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

    /// Record endpoint selection, and separately the use of the pinning
    /// header so its adoption is visible on its own.
    pub fn record_endpoint_selection(
        &self,
        upstream_id: &str,
        endpoint_host: &str,
        selection_method: &'static str,
    ) {
        self.routing_endpoint_selected.add(
            1,
            &[
                KeyValue::new("upstream_id", upstream_id.to_owned()),
                KeyValue::new("endpoint_host", endpoint_host.to_owned()),
                KeyValue::new("selection_method", selection_method),
            ],
        );
        if selection_method == "explicit_header" {
            self.routing_target_host_used.add(
                1,
                &[
                    KeyValue::new("upstream_id", upstream_id.to_owned()),
                    KeyValue::new("endpoint_host", endpoint_host.to_owned()),
                ],
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_normalization_bounds_cardinality() {
        assert_eq!(normalize_method("get"), "GET");
        assert_eq!(normalize_method("PATCH"), "PATCH");
        assert_eq!(normalize_method("PROPFIND"), "_OTHER");
    }

    #[test]
    fn instruments_can_be_built_and_recorded_without_a_provider() {
        let metrics = OagwMetrics::from_global();
        metrics.record_request("api.example.com", "GET", "/v1/models", 200);
        metrics.record_duration("api.example.com", "/v1/models", "upstream", 0.01);
        metrics.adjust_in_flight("api.example.com", 1);
        metrics.adjust_in_flight("api.example.com", -1);
        metrics.record_error("api.example.com", "/v1/models", "route_not_found");
        metrics.record_rate_limited("api.example.com", "/v1/models");
        metrics.record_endpoint_selection("u1", "api.example.com", "explicit_header");
    }

    #[test]
    fn duration_buckets_match_the_design() {
        assert_eq!(DURATION_BUCKETS.len(), 12);
        assert!((DURATION_BUCKETS[0] - 0.001).abs() < f64::EPSILON);
        assert!((DURATION_BUCKETS[11] - 10.0).abs() < f64::EPSILON);
    }
}
