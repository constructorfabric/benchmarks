//! Data Plane instrumentation (`cpt-cf-oagw-nfr-observability`).
//!
//! Label keys follow the OTel HTTP semantic conventions so OAGW and the
//! inbound API Gateway share dashboards. There are deliberately no tenant
//! labels — `host` carries the upstream alias instead.

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, Meter, UpDownCounter};

/// Duration buckets from `DESIGN.md` §4.2.
pub const DURATION_BUCKETS: &[f64] = &[
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

pub struct OagwMetrics {
    requests_total: Counter<u64>,
    request_duration: Histogram<f64>,
    requests_in_flight: UpDownCounter<i64>,
    errors_total: Counter<u64>,
    rate_limit_exceeded_total: Counter<u64>,
    routing_target_host_used: Counter<u64>,
    routing_endpoint_selected: Counter<u64>,
}

impl OagwMetrics {
    #[must_use]
    pub fn from_global() -> Self {
        Self::new(&opentelemetry::global::meter("oagw"))
    }

    #[must_use]
    pub fn new(meter: &Meter) -> Self {
        Self {
            requests_total: meter.u64_counter("oagw_requests_total").build(),
            request_duration: meter
                .f64_histogram("oagw_request_duration_seconds")
                .with_boundaries(DURATION_BUCKETS.to_vec())
                .build(),
            requests_in_flight: meter.i64_up_down_counter("oagw_requests_in_flight").build(),
            errors_total: meter.u64_counter("oagw_errors_total").build(),
            rate_limit_exceeded_total: meter
                .u64_counter("oagw_rate_limit_exceeded_total")
                .build(),
            routing_target_host_used: meter.u64_counter("oagw_routing_target_host_used").build(),
            routing_endpoint_selected: meter.u64_counter("oagw_routing_endpoint_selected").build(),
        }
    }

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

    pub fn record_duration(&self, host: &str, route: &str, phase: &str, seconds: f64) {
        self.request_duration.record(
            seconds,
            &[
                KeyValue::new("host", host.to_owned()),
                KeyValue::new("http.route", route.to_owned()),
                KeyValue::new("phase", phase.to_owned()),
            ],
        );
    }

    pub fn inc_in_flight(&self, host: &str) {
        self.requests_in_flight
            .add(1, &[KeyValue::new("host", host.to_owned())]);
    }

    pub fn dec_in_flight(&self, host: &str) {
        self.requests_in_flight
            .add(-1, &[KeyValue::new("host", host.to_owned())]);
    }

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

    pub fn record_rate_limited(&self, host: &str, path: &str) {
        self.rate_limit_exceeded_total.add(
            1,
            &[
                KeyValue::new("host", host.to_owned()),
                KeyValue::new("path", path.to_owned()),
            ],
        );
    }

    pub fn record_target_host_used(&self, upstream_id: &str, endpoint_host: &str) {
        self.routing_target_host_used.add(
            1,
            &[
                KeyValue::new("upstream_id", upstream_id.to_owned()),
                KeyValue::new("endpoint_host", endpoint_host.to_owned()),
            ],
        );
    }

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

/// Cardinality guard: a non-standard verb becomes `_OTHER` (OTel semconv).
#[must_use]
pub fn normalize_method(method: &str) -> String {
    const STANDARD: &[&str] = &[
        "GET", "HEAD", "POST", "PUT", "DELETE", "CONNECT", "OPTIONS", "TRACE", "PATCH",
    ];
    let upper = method.to_ascii_uppercase();
    if STANDARD.contains(&upper.as_str()) {
        upper
    } else {
        "_OTHER".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_standard_verbs_collapse_to_other() {
        assert_eq!(normalize_method("get"), "GET");
        assert_eq!(normalize_method("PATCH"), "PATCH");
        assert_eq!(normalize_method("FROBNICATE"), "_OTHER");
    }

    #[test]
    fn duration_buckets_match_the_design_document() {
        assert_eq!(DURATION_BUCKETS.first().copied(), Some(0.001));
        assert_eq!(DURATION_BUCKETS.last().copied(), Some(10.0));
        assert_eq!(DURATION_BUCKETS.len(), 12);
    }

    #[test]
    fn metrics_can_be_built_from_the_global_meter() {
        let metrics = OagwMetrics::from_global();
        // Recording against the default no-op meter must not panic.
        metrics.record_request("api.openai.com", "post", "/v1/chat", 200);
        metrics.inc_in_flight("api.openai.com");
        metrics.dec_in_flight("api.openai.com");
        metrics.record_error("api.openai.com", "/v1/chat", "timeout");
        metrics.record_rate_limited("api.openai.com", "/v1/chat");
        metrics.record_duration("api.openai.com", "/v1/chat", "total", 0.01);
        metrics.record_target_host_used("u", "us.vendor.com");
        metrics.record_endpoint_selected("u", "us.vendor.com", "round_robin");
    }
}
