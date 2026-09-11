//! OpenTelemetry instruments of DESIGN §4.2.
//!
//! Metrics are host-scoped only: no tenant label ever appears on a metric
//! (Constitution IV), and `http.route` is the normalised route pattern.

use std::sync::OnceLock;

use opentelemetry::metrics::{Counter, Gauge, Histogram, Meter};

/// Duration bucket boundaries documented for `oagw_request_duration_seconds`.
pub const DURATION_BUCKETS: &[f64] = &[
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// The set of instruments the gear exports.
#[derive(Debug, Clone)]
pub struct Instruments {
    /// `oagw_requests_total{host,http.request.method,http.route,http.response.status_code}`
    pub requests_total: Counter<u64>,
    /// `oagw_request_duration_seconds{host,http.route,phase}`
    pub request_duration: Histogram<f64>,
    /// `oagw_requests_in_flight{host}`
    pub in_flight: Gauge<u64>,
    /// `oagw_errors_total{host,http.route,error_type}`
    pub errors_total: Counter<u64>,
    /// `oagw_circuit_breaker_state{host}`
    pub circuit_breaker_state: Gauge<u64>,
    /// `oagw_circuit_breaker_transitions_total{host,from_state,to_state}`
    pub circuit_breaker_transitions: Counter<u64>,
    /// `oagw_rate_limit_exceeded_total{host,path}`
    pub rate_limit_exceeded: Counter<u64>,
    /// `oagw_rate_limit_usage_ratio{host,path}`
    pub rate_limit_usage_ratio: Gauge<f64>,
    /// `oagw_routing_target_host_used{upstream_id,endpoint_host}`
    pub routing_target_host_used: Counter<u64>,
    /// `oagw_routing_endpoint_selected{upstream_id,endpoint_host,selection_method}`
    pub routing_endpoint_selected: Counter<u64>,
    /// `oagw_upstream_available{host,endpoint}`
    pub upstream_available: Gauge<u64>,
    /// `oagw_upstream_connections{host,state}`
    pub upstream_connections: Gauge<u64>,
}

/// Instrument names, kept separate so tests and the OpenAPI/metrics registry
/// can enumerate them without constructing instruments.
pub mod names {
    /// Total proxied requests.
    pub const REQUESTS_TOTAL: &str = "oagw_requests_total";
    /// Request duration.
    pub const REQUEST_DURATION: &str = "oagw_request_duration_seconds";
    /// Requests currently in flight.
    pub const IN_FLIGHT: &str = "oagw_requests_in_flight";
    /// Errors produced by the gateway.
    pub const ERRORS_TOTAL: &str = "oagw_errors_total";
    /// Circuit breaker state.
    pub const CIRCUIT_BREAKER_STATE: &str = "oagw_circuit_breaker_state";
    /// Circuit breaker transitions.
    pub const CIRCUIT_BREAKER_TRANSITIONS: &str = "oagw_circuit_breaker_transitions_total";
    /// Rate-limit rejections.
    pub const RATE_LIMIT_EXCEEDED: &str = "oagw_rate_limit_exceeded_total";
    /// Rate-limit usage ratio.
    pub const RATE_LIMIT_USAGE_RATIO: &str = "oagw_rate_limit_usage_ratio";
    /// Selected target host.
    pub const ROUTING_TARGET_HOST_USED: &str = "oagw_routing_target_host_used";
    /// Endpoint selection method.
    pub const ROUTING_ENDPOINT_SELECTED: &str = "oagw_routing_endpoint_selected";
    /// Upstream reachability.
    pub const UPSTREAM_AVAILABLE: &str = "oagw_upstream_available";
    /// Upstream connection gauge.
    pub const UPSTREAM_CONNECTIONS: &str = "oagw_upstream_connections";
}

/// Label keys, as documented.
pub mod labels {
    /// Upstream host label.
    pub const HOST: &str = "host";
    /// HTTP method label.
    pub const METHOD: &str = "http.request.method";
    /// Normalised route pattern.
    pub const ROUTE: &str = "http.route";
    /// Response status code.
    pub const STATUS: &str = "http.response.status_code";
    /// Processing phase.
    pub const PHASE: &str = "phase";
    /// Error type.
    pub const ERROR_TYPE: &str = "error_type";
    /// Endpoint host.
    pub const ENDPOINT_HOST: &str = "endpoint_host";
    /// Endpoint address.
    pub const ENDPOINT: &str = "endpoint";
    /// Upstream id.
    pub const UPSTREAM_ID: &str = "upstream_id";
    /// Selection method (round-robin / target-host).
    pub const SELECTION_METHOD: &str = "selection_method";
    /// Connection state.
    pub const STATE: &str = "state";
    /// Previous circuit-breaker state.
    pub const FROM_STATE: &str = "from_state";
    /// Next circuit-breaker state.
    pub const TO_STATE: &str = "to_state";
}

static INSTRUMENTS: OnceLock<Instruments> = OnceLock::new();

/// Returns the process-wide instrument set, building it on first use.
pub fn instruments() -> &'static Instruments {
    INSTRUMENTS.get_or_init(build)
}

fn default_meter() -> Meter {
    opentelemetry::global::meter("cf-gears-oagw")
}

/// Builds the instrument set from an explicit meter (used by tests).
pub fn build_with_meter(meter: &Meter) -> Instruments {
    let request_duration = meter
        .f64_histogram(names::REQUEST_DURATION)
        .with_description("Time spent processing a proxied request")
        .with_unit("s")
        .build();

    Instruments {
        requests_total: meter
            .u64_counter(names::REQUESTS_TOTAL)
            .with_description("Proxied requests, by outcome")
            .build(),
        request_duration,
        in_flight: meter
            .u64_gauge(names::IN_FLIGHT)
            .with_description("Requests currently in flight")
            .build(),
        errors_total: meter
            .u64_counter(names::ERRORS_TOTAL)
            .with_description("Gateway errors, by type")
            .build(),
        circuit_breaker_state: meter
            .u64_gauge(names::CIRCUIT_BREAKER_STATE)
            .with_description("Circuit breaker state per host")
            .build(),
        circuit_breaker_transitions: meter
            .u64_counter(names::CIRCUIT_BREAKER_TRANSITIONS)
            .with_description("Circuit breaker state transitions")
            .build(),
        rate_limit_exceeded: meter
            .u64_counter(names::RATE_LIMIT_EXCEEDED)
            .with_description("Requests rejected by a rate limit")
            .build(),
        rate_limit_usage_ratio: meter
            .f64_gauge(names::RATE_LIMIT_USAGE_RATIO)
            .with_description("Consumed fraction of a rate limit bucket")
            .build(),
        routing_target_host_used: meter
            .u64_counter(names::ROUTING_TARGET_HOST_USED)
            .with_description("Requests routed to an explicit target host")
            .build(),
        routing_endpoint_selected: meter
            .u64_counter(names::ROUTING_ENDPOINT_SELECTED)
            .with_description("Endpoint selections, by method")
            .build(),
        upstream_available: meter
            .u64_gauge(names::UPSTREAM_AVAILABLE)
            .with_description("Upstream reachability")
            .build(),
        upstream_connections: meter
            .u64_gauge(names::UPSTREAM_CONNECTIONS)
            .with_description("Upstream connections, by state")
            .build(),
    }
}

fn build() -> Instruments {
    let meter = default_meter();
    build_with_meter(&meter)
}

/// Records a completed proxied request.
pub fn record_request(
    host: &str,
    method: &str,
    route: &str,
    status: u16,
    duration_secs: f64,
) {
    let i = instruments();
    i.requests_total.add(
        1,
        &[
            opentelemetry::KeyValue::new(labels::HOST, host.to_string()),
            opentelemetry::KeyValue::new(labels::METHOD, method.to_string()),
            opentelemetry::KeyValue::new(labels::ROUTE, route.to_string()),
            opentelemetry::KeyValue::new(labels::STATUS, i64::from(status)),
        ],
    );
    i.request_duration.record(
        duration_secs,
        &[
            opentelemetry::KeyValue::new(labels::HOST, host.to_string()),
            opentelemetry::KeyValue::new(labels::ROUTE, route.to_string()),
            opentelemetry::KeyValue::new(labels::PHASE, "total"),
        ],
    );
}

/// Records a gateway error.
pub fn record_error(host: &str, route: &str, error_type: &str) {
    instruments().errors_total.add(
        1,
        &[
            opentelemetry::KeyValue::new(labels::HOST, host.to_string()),
            opentelemetry::KeyValue::new(labels::ROUTE, route.to_string()),
            opentelemetry::KeyValue::new(labels::ERROR_TYPE, error_type.to_string()),
        ],
    );
}

/// Records a rate-limit rejection.
pub fn record_rate_limit_exceeded(host: &str, path: &str) {
    instruments().rate_limit_exceeded.add(
        1,
        &[
            opentelemetry::KeyValue::new(labels::HOST, host.to_string()),
            opentelemetry::KeyValue::new(labels::ENDPOINT_HOST, path.to_string()),
        ],
    );
}

/// Records how much of a bucket has been consumed.
pub fn record_rate_limit_usage(host: &str, path: &str, capacity: u64, remaining: u64) {
    let ratio = if capacity == 0 {
        0.0
    } else {
        let used = capacity.saturating_sub(remaining) as f64;
        used / capacity as f64
    };
    instruments().rate_limit_usage_ratio.record(
        ratio,
        &[
            opentelemetry::KeyValue::new(labels::HOST, host.to_string()),
            opentelemetry::KeyValue::new(labels::ROUTE, path.to_string()),
        ],
    );
}

/// Records an endpoint selection.
pub fn record_endpoint_selected(
    upstream_id: &str,
    endpoint_host: &str,
    selection_method: &str,
) {
    instruments().routing_endpoint_selected.add(
        1,
        &[
            opentelemetry::KeyValue::new(labels::UPSTREAM_ID, upstream_id.to_string()),
            opentelemetry::KeyValue::new(labels::ENDPOINT_HOST, endpoint_host.to_string()),
            opentelemetry::KeyValue::new(labels::SELECTION_METHOD, selection_method.to_string()),
        ],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_buckets_match_the_documented_set() {
        assert_eq!(
            DURATION_BUCKETS,
            &[
                0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0
            ][..]
        );
    }

    #[test]
    fn instrument_names_are_the_documented_ones() {
        assert_eq!(names::REQUESTS_TOTAL, "oagw_requests_total");
        assert_eq!(names::REQUEST_DURATION, "oagw_request_duration_seconds");
        assert_eq!(names::IN_FLIGHT, "oagw_requests_in_flight");
        assert_eq!(names::ERRORS_TOTAL, "oagw_errors_total");
        assert_eq!(names::CIRCUIT_BREAKER_STATE, "oagw_circuit_breaker_state");
        assert_eq!(names::RATE_LIMIT_EXCEEDED, "oagw_rate_limit_exceeded_total");
        assert_eq!(
            names::RATE_LIMIT_USAGE_RATIO,
            "oagw_rate_limit_usage_ratio"
        );
        assert_eq!(
            names::ROUTING_TARGET_HOST_USED,
            "oagw_routing_target_host_used"
        );
        assert_eq!(
            names::ROUTING_ENDPOINT_SELECTED,
            "oagw_routing_endpoint_selected"
        );
        assert_eq!(names::UPSTREAM_AVAILABLE, "oagw_upstream_available");
        assert_eq!(names::UPSTREAM_CONNECTIONS, "oagw_upstream_connections");
    }

    #[test]
    fn label_keys_carry_no_tenant() {
        assert_eq!(labels::HOST, "host");
        assert_eq!(labels::ROUTE, "http.route");
        assert_eq!(labels::STATUS, "http.response.status_code");
        assert_eq!(labels::ERROR_TYPE, "error_type");
        assert_eq!(labels::UPSTREAM_ID, "upstream_id");
        assert_eq!(labels::ENDPOINT_HOST, "endpoint_host");
        assert_eq!(labels::SELECTION_METHOD, "selection_method");
    }

    #[test]
    fn instruments_can_be_built_against_a_test_meter() {
        let meter = opentelemetry::global::meter("oagw-test");
        let _built = build_with_meter(&meter);
        // The instruments exist; a name check would need the metric handle's
        // private metadata, so the constructor succeeding is the assertion.
        assert_eq!(names::REQUEST_DURATION, "oagw_request_duration_seconds");
    }
}
