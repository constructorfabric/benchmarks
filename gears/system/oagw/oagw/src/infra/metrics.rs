//! Data-plane metrics (DESIGN §4.2 "Metrics and Observability").
//!
//! Instruments are created once via the process global meter and recorded
//! with OTel semantic-convention attribute keys so the OAGW and API-Gateway
//! dashboards stay aligned. `host` carries the upstream alias; `http.route`
//! is the matched route pattern (not the raw path). No tenant labels are
//! recorded (cardinality management).

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, UpDownCounter};

/// Instrument holder for the OAGW data plane.
pub struct OagwMetrics {
    /// `oagw_requests_total{host, http.request.method, http.route, http.response.status_code}`
    requests_total: Counter<u64>,
    /// `oagw_request_duration_seconds{host, http.route, phase}` histogram.
    duration_seconds: Histogram<f64>,
    /// `oagw_requests_in_flight{host}` gauge.
    requests_in_flight: UpDownCounter<i64>,
    /// `oagw_errors_total{host, http.route, error_type}` counter.
    errors_total: Counter<u64>,
    /// `oagw_rate_limit_exceeded_total{host, path}` counter.
    rate_limit_exceeded_total: Counter<u64>,
    /// `oagw_routing_target_host_used{upstream_id, endpoint_host}` counter.
    routing_target_host_used: Counter<u64>,
    /// `oagw_routing_endpoint_selected{upstream_id, endpoint_host, selection_method}` counter.
    routing_endpoint_selected: Counter<u64>,
}

impl OagwMetrics {
    /// Build the metrics set under the OAGW instrument scope.
    #[must_use]
    pub fn new() -> Self {
        let scope = opentelemetry::InstrumentationScope::builder("oagw").build();
        let meter = opentelemetry::global::meter_with_scope(scope);

        // DESIGN §4.2 specifies explicit histogram buckets
        // [0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0].
        // opentelemetry 0.32 builders do not expose custom bucket boundaries,
        // so the SDK default buckets are used (a monitoring-side concern).
        let duration_seconds = meter
            .f64_histogram("oagw_request_duration_seconds")
            .with_description("Outbound proxy request duration, in seconds, by phase.")
            .with_unit("s")
            .build();

        Self {
            requests_total: meter
                .u64_counter("oagw_requests_total")
                .with_description("Total proxied (data-plane) requests.")
                .build(),
            duration_seconds,
            requests_in_flight: meter
                .i64_up_down_counter("oagw_requests_in_flight")
                .with_description("Proxy requests currently in flight.")
                .build(),
            errors_total: meter
                .u64_counter("oagw_errors_total")
                .with_description("Total gateway-originated proxy errors.")
                .build(),
            rate_limit_exceeded_total: meter
                .u64_counter("oagw_rate_limit_exceeded_total")
                .with_description("Requests rejected by the rate limiter.")
                .build(),
            routing_target_host_used: meter
                .u64_counter("oagw_routing_target_host_used")
                .with_description("Requests pinned via X-OAGW-Target-Host.")
                .build(),
            routing_endpoint_selected: meter
                .u64_counter("oagw_routing_endpoint_selected")
                .with_description("Endpoint selection for proxied requests.")
                .build(),
        }
    }

    /// Record the completion of a proxied request.
    pub fn record_request(&self, host: &str, route: &str, method: &str, status: u16) {
        self.requests_total.add(
            1,
            &[
                KeyValue::new("host", host.to_owned()),
                KeyValue::new("http.request.method", normalize_method(method)),
                KeyValue::new("http.route", route.to_owned()),
                KeyValue::new("http.response.status_code", status as i64),
            ],
        );
    }

    /// Record the duration of one phase.
    pub fn record_duration(&self, host: &str, route: &str, phase: &str, secs: f64) {
        self.duration_seconds.record(
            secs,
            &[
                KeyValue::new("host", host.to_owned()),
                KeyValue::new("http.route", route.to_owned()),
                KeyValue::new("phase", phase.to_owned()),
            ],
        );
    }

    /// Adjust the in-flight gauge (`delta` is +1 on enter, -1 on exit).
    pub fn adjust_in_flight(&self, host: &str, delta: i64) {
        self.requests_in_flight
            .add(delta, &[KeyValue::new("host", host.to_owned())]);
    }

    /// Record a gateway error categorized by its GTS instance name.
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
    pub fn record_rate_limit_exceeded(&self, host: &str, path: &str) {
        self.rate_limit_exceeded_total.add(
            1,
            &[
                KeyValue::new("host", host.to_owned()),
                KeyValue::new("path", path.to_owned()),
            ],
        );
    }

    /// Record explicit `X-OAGW-Target-Host` pinning.
    pub fn record_target_host_used(&self, upstream_id: &str, endpoint_host: &str) {
        self.routing_target_host_used.add(
            1,
            &[
                KeyValue::new("upstream_id", upstream_id.to_owned()),
                KeyValue::new("endpoint_host", endpoint_host.to_owned()),
            ],
        );
    }

    /// Record endpoint selection for a pool.
    pub fn record_endpoint_selected(
        &self,
        upstream_id: &str,
        endpoint_host: &str,
        selection_method: &str,
    ) {
        self.routing_endpoint_selected.add(
            1,
            &[
                KeyValue::new("upstream_id", upstream_id.to_owned()),
                KeyValue::new("endpoint_host", endpoint_host.to_owned()),
                KeyValue::new("selection_method", selection_method.to_owned()),
            ],
        );
    }
}

impl Default for OagwMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Normalize a method token to a standard verb or `_OTHER` (OTel HTTP semconv).
fn normalize_method(method: &str) -> String {
    let upper = method.to_ascii_uppercase();
    match upper.as_str() {
        "GET" | "POST" | "PUT" | "DELETE" | "PATCH" | "HEAD" | "OPTIONS" | "CONNECT" | "TRACE" => {
            upper
        }
        _ => "_OTHER".to_owned(),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn method_normalization() {
        assert_eq!(normalize_method("get"), "GET");
        assert_eq!(normalize_method("POST"), "POST");
        assert_eq!(normalize_method("BREW"), "_OTHER");
        assert_eq!(normalize_method("propfind"), "_OTHER");
    }
}
