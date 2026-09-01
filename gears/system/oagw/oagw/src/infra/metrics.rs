//! OpenTelemetry metric instruments (Prometheus names per DESIGN §4.2).
//!
//! All instruments live in one struct so the Data Plane holds a single handle;
//! label cardinality is bounded because `host` is the upstream alias and
//! `http.route` is the normalized match pattern, never the raw request path.

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Gauge, Histogram};

/// Request-duration histogram buckets (seconds), DESIGN §4.2.
pub const DURATION_BUCKETS: [f64; 12] = [
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// A gateway error observation, before labelling.
#[derive(Debug, Clone, Copy)]
pub struct ErrorLabels<'a> {
    /// Upstream alias.
    pub host: &'a str,
    /// Normalized route pattern (or alias root when unmatched).
    pub route: &'a str,
    /// Gateway error code.
    pub error_code: &'a str,
}

/// Aggregate metric instruments for the OAGW Data Plane.
#[derive(Clone)]
pub struct OagwMetrics {
    requests_total: Counter<u64>,
    request_duration: Histogram<f64>,
    requests_in_flight: Gauge<i64>,
    errors_total: Counter<u64>,
    circuit_state: Gauge<i64>,
    circuit_transitions: Counter<u64>,
    rate_limit_exceeded: Counter<u64>,
    rate_limit_usage: Gauge<f64>,
    target_host_used: Counter<u64>,
    endpoint_selected: Counter<u64>,
    upstream_available: Gauge<i64>,
}

impl std::fmt::Debug for OagwMetrics {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("OagwMetrics").finish()
    }
}

impl Default for OagwMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl OagwMetrics {
    /// Creates the instrument set from the global meter provider.
    #[must_use]
    pub fn new() -> Self {
        let meter = opentelemetry::global::meter("cf-gears-oagw");
        Self {
            requests_total: meter
                .u64_counter("oagw_requests_total")
                .with_description("Proxied requests by host, method, route and status.")
                .build(),
            request_duration: meter
                .f64_histogram("oagw_request_duration_seconds")
                .with_boundaries(DURATION_BUCKETS.to_vec())
                .with_description("End to end proxy duration by host, route and phase.")
                .build(),
            requests_in_flight: meter
                .i64_gauge("oagw_requests_in_flight")
                .with_description("Requests currently proxied per host.")
                .build(),
            errors_total: meter
                .u64_counter("oagw_errors_total")
                .with_description("Gateway errors by host, route and error type.")
                .build(),
            circuit_state: meter
                .i64_gauge("oagw_circuit_breaker_state")
                .with_description("Circuit breaker state per host (0 closed, 1 half-open, 2 open).")
                .build(),
            circuit_transitions: meter
                .u64_counter("oagw_circuit_breaker_transitions_total")
                .with_description("Circuit breaker state transitions per host.")
                .build(),
            rate_limit_exceeded: meter
                .u64_counter("oagw_rate_limit_exceeded_total")
                .with_description("Rejected requests by host and path prefix.")
                .build(),
            rate_limit_usage: meter
                .f64_gauge("oagw_rate_limit_usage_ratio")
                .with_description("Token bucket fill ratio per host and path prefix.")
                .build(),
            target_host_used: meter
                .u64_counter("oagw_routing_target_host_used")
                .with_description("Target host selections per upstream and endpoint.")
                .build(),
            endpoint_selected: meter
                .u64_counter("oagw_routing_endpoint_selected")
                .with_description("Endpoint selections per upstream and selection method.")
                .build(),
            upstream_available: meter
                .i64_gauge("oagw_upstream_available")
                .with_description("Endpoint reachability per host (0 down, 1 up).")
                .build(),
        }
    }

    /// Records an admitted request.
    pub fn record_request(
        &self,
        host: &str,
        route: &str,
        method: &str,
        status: u16,
        duration_seconds: f64,
    ) {
        let base = [
            KeyValue::new("host", host.to_owned()),
            KeyValue::new("http.route", route.to_owned()),
            KeyValue::new("http.request.method", normalize_method(method)),
        ];
        self.requests_total.add(
            1,
            &[
                base[0].clone(),
                base[1].clone(),
                base[2].clone(),
                KeyValue::new("http.response.status_code", i64::from(status)),
            ],
        );
        self.request_duration.record(
            duration_seconds,
            &[
                base[0].clone(),
                base[1].clone(),
                KeyValue::new("phase", "total"),
            ],
        );
    }

    /// Publishes the current in-flight request count for a host.
    pub fn in_flight(&self, host: &str, current: i64) {
        self.requests_in_flight
            .record(current, &[KeyValue::new("host", host.to_owned())]);
    }

    /// Records a gateway-side failure.
    pub fn record_error(&self, labels: ErrorLabels<'_>) {
        self.errors_total.add(
            1,
            &[
                KeyValue::new("host", labels.host.to_owned()),
                KeyValue::new("http.route", labels.route.to_owned()),
                KeyValue::new("error_type", labels.error_code.to_owned()),
            ],
        );
    }

    /// Publishes the breaker state (`0` closed, `1` half-open, `2` open).
    pub fn circuit_state(&self, host: &str, state: crate::infra::ratelimit::CircuitState) {
        let value = match state {
            crate::infra::ratelimit::CircuitState::Closed => 0,
            crate::infra::ratelimit::CircuitState::HalfOpen => 1,
            crate::infra::ratelimit::CircuitState::Open => 2,
        };
        self.circuit_state
            .record(value, &[KeyValue::new("host", host.to_owned())]);
    }

    /// Records a breaker transition.
    pub fn circuit_transition(&self, host: &str, from: &str, to: &str) {
        self.circuit_transitions.add(
            1,
            &[
                KeyValue::new("host", host.to_owned()),
                KeyValue::new("from_state", from.to_owned()),
                KeyValue::new("to_state", to.to_owned()),
            ],
        );
    }

    /// Records a rejected request and the bucket fill ratio.
    pub fn rate_limit_exceeded(&self, host: &str, path: &str, usage_ratio: f64) {
        let labels = [
            KeyValue::new("host", host.to_owned()),
            KeyValue::new("path", path.to_owned()),
        ];
        self.rate_limit_exceeded.add(1, &labels);
        self.rate_limit_usage.record(usage_ratio, &labels);
    }

    /// Records `X-OAGW-Target-Host` usage.
    pub fn target_host_used(&self, upstream_id: &str, endpoint_host: &str) {
        self.target_host_used.add(
            1,
            &[
                KeyValue::new("upstream_id", upstream_id.to_owned()),
                KeyValue::new("endpoint_host", endpoint_host.to_owned()),
            ],
        );
    }

    /// Records an endpoint selection.
    pub fn endpoint_selected(&self, upstream_id: &str, endpoint_host: &str, method: &str) {
        self.endpoint_selected.add(
            1,
            &[
                KeyValue::new("upstream_id", upstream_id.to_owned()),
                KeyValue::new("endpoint_host", endpoint_host.to_owned()),
                KeyValue::new("selection_method", method.to_owned()),
            ],
        );
    }

    /// Publishes endpoint reachability.
    pub fn upstream_available(&self, host: &str, endpoint: &str, available: bool) {
        self.upstream_available.record(
            i64::from(u8::from(available)),
            &[
                KeyValue::new("host", host.to_owned()),
                KeyValue::new("endpoint", endpoint.to_owned()),
            ],
        );
    }
}

/// Normalizes an HTTP method to an OTel semantic-convention token.
#[must_use]
pub fn normalize_method(method: &str) -> String {
    match method {
        "GET" | "HEAD" | "POST" | "PUT" | "DELETE" | "CONNECT" | "OPTIONS" | "TRACE" | "PATCH" => {
            method.to_owned()
        }
        _ => "_OTHER".to_owned(),
    }
}

/// Circuit-state label used by `oagw_circuit_breaker_transitions_total`.
#[must_use]
pub fn state_label(state: crate::infra::ratelimit::CircuitState) -> &'static str {
    match state {
        crate::infra::ratelimit::CircuitState::Closed => "closed",
        crate::infra::ratelimit::CircuitState::HalfOpen => "half_open",
        crate::infra::ratelimit::CircuitState::Open => "open",
    }
}

#[cfg(test)]
#[path = "metrics_tests.rs"]
mod tests;
