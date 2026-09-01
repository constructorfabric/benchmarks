// Created: 2026-08-29 by Constructor Tech
//! OpenTelemetry adapter implementing [`OagwMetricsPort`].
//!
//! Instruments are pulled from the process-global meter provider installed by
//! the host; a no-op until an exporter is wired. Instrument names are full
//! literal Prometheus names: counters end in `_total`, duration histograms in
//! `_seconds`, with suffixes baked in (no `.with_unit()`), matching the
//! platform's `add_metric_suffixes: false` collector posture.
//!
//! Cardinality follows DESIGN §4.2: no tenant labels, `http.route` is the
//! normalized route match pattern (never the raw request path), method is
//! normalized to a standard verb or `_OTHER`, and `host` is the upstream alias.

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Gauge, Histogram, Meter};

use crate::domain::ports::metrics::{BreakerState, OagwMetricsPort, SelectionMethod};

/// Meter / instrumentation scope name.
const METER_NAME: &str = "oagw";

// ─── Metric names (literal Prometheus form; `add_metric_suffixes: false`) ─────
const REQUESTS: &str = "oagw_requests_total";
const REQUEST_DURATION: &str = "oagw_request_duration_seconds";
const ERRORS: &str = "oagw_errors_total";
const RATE_LIMIT_EXCEEDED: &str = "oagw_rate_limit_exceeded_total";
const BREAKER_TRANSITIONS: &str = "oagw_circuit_breaker_transitions_total";
const TARGET_HOST_USED: &str = "oagw_routing_target_host_used";
const ENDPOINT_SELECTED: &str = "oagw_routing_endpoint_selected";

/// OpenTelemetry-backed metrics handle for the `oagw` module.
pub struct OagwMetricsMeter {
    requests: Counter<u64>,
    request_duration: Histogram<f64>,
    errors: Counter<u64>,
    rate_limit_exceeded: Counter<u64>,
    breaker_state: Gauge<i64>,
    breaker_transitions: Counter<u64>,
    target_host_used: Counter<u64>,
    endpoint_selected: Counter<u64>,
}

impl std::fmt::Debug for OagwMetricsMeter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OagwMetricsMeter").finish_non_exhaustive()
    }
}

/// Histogram boundaries of the request-duration instrument (DESIGN §4.2).
const DURATION_BUCKETS: [f64; 12] = [
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Verbs the OTel HTTP semantic conventions name.
const STANDARD_METHODS: [&str; 9] = [
    "GET", "HEAD", "POST", "PUT", "DELETE", "CONNECT", "OPTIONS", "TRACE", "PATCH",
];

/// `http.request.method` normalization of the OTel HTTP semantic conventions.
fn normalized_method(method: &str) -> &str {
    // A case-variant of a standard verb reports the verb itself; anything else
    // the convention does not name collapses into `_OTHER`.
    STANDARD_METHODS
        .iter()
        .copied()
        .find(|standard| method.eq_ignore_ascii_case(standard))
        .unwrap_or("_OTHER")
}

impl OagwMetricsMeter {
    /// Build the instrument set from the supplied meter.
    #[must_use]
    pub fn new(meter: &Meter) -> Self {
        Self {
            requests: meter
                .u64_counter(REQUESTS)
                .with_description("Proxied requests by upstream, route, method and status")
                .build(),
            request_duration: meter
                .f64_histogram(REQUEST_DURATION)
                .with_description("End-to-end proxied request duration, by phase")
                .with_boundaries(DURATION_BUCKETS.to_vec())
                .build(),
            errors: meter
                .u64_counter(ERRORS)
                .with_description("Gateway rejections by error type")
                .build(),
            rate_limit_exceeded: meter
                .u64_counter(RATE_LIMIT_EXCEEDED)
                .with_description("Requests refused because their budget was exhausted")
                .build(),
            breaker_state: meter
                .i64_gauge("oagw_circuit_breaker_state")
                .with_description("Blocking state of an upstream endpoint's breaker")
                .build(),
            breaker_transitions: meter
                .u64_counter(BREAKER_TRANSITIONS)
                .with_description("Circuit-breaker transitions by from and to state")
                .build(),
            target_host_used: meter
                .u64_counter(TARGET_HOST_USED)
                .with_description("Requests that named their endpoint with X-OAGW-Target-Host")
                .build(),
            endpoint_selected: meter
                .u64_counter(ENDPOINT_SELECTED)
                .with_description("Endpoint selections by selection method")
                .build(),
        }
    }

    /// Build the instrument set from the process-global meter provider.
    #[must_use]
    pub fn from_global() -> Self {
        Self::new(&opentelemetry::global::meter(METER_NAME))
    }
}

impl OagwMetricsPort for OagwMetricsMeter {
    fn record_request(
        &self,
        host: &str,
        route: &str,
        method: &str,
        status_code: u16,
        duration_seconds: f64,
    ) {
        let attributes = [
            KeyValue::new("host", host.to_owned()),
            KeyValue::new("http.route", route.to_owned()),
            KeyValue::new("http.request.method", normalized_method(method).to_owned()),
            KeyValue::new("http.response.status_code", i64::from(status_code)),
        ];
        self.requests.add(1, &attributes);
        self.request_duration.record(duration_seconds, &attributes);
    }

    fn record_error(&self, host: &str, route: &str, error_type: &str) {
        self.errors.add(
            1,
            &[
                KeyValue::new("host", host.to_owned()),
                KeyValue::new("http.route", route.to_owned()),
                KeyValue::new("error_type", error_type.to_owned()),
            ],
        );
    }

    fn record_rate_limit_exceeded(&self, host: &str, path: &str) {
        self.rate_limit_exceeded.add(
            1,
            &[
                KeyValue::new("host", host.to_owned()),
                KeyValue::new("path", path.to_owned()),
            ],
        );
    }

    fn record_breaker_transition(&self, host: &str, from: BreakerState, to: BreakerState) {
        self.breaker_transitions.add(
            1,
            &[
                KeyValue::new("host", host.to_owned()),
                KeyValue::new("from_state", from.as_str().to_owned()),
                KeyValue::new("to_state", to.as_str().to_owned()),
            ],
        );
    }

    fn set_breaker_state(&self, host: &str, state: BreakerState) {
        self.breaker_state.record(
            match state {
                BreakerState::Closed => 0,
                BreakerState::Open => 1,
            },
            &[KeyValue::new("host", host.to_owned())],
        );
    }

    fn record_target_host_used(&self, upstream_id: &str, endpoint_host: &str) {
        self.target_host_used.add(
            1,
            &[
                KeyValue::new("upstream_id", upstream_id.to_owned()),
                KeyValue::new("endpoint_host", endpoint_host.to_owned()),
            ],
        );
    }

    fn record_endpoint_selected(
        &self,
        upstream_id: &str,
        endpoint_host: &str,
        method: SelectionMethod,
    ) {
        self.endpoint_selected.add(
            1,
            &[
                KeyValue::new("upstream_id", upstream_id.to_owned()),
                KeyValue::new("endpoint_host", endpoint_host.to_owned()),
                KeyValue::new("selection_method", method.as_str().to_owned()),
            ],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_verbs_are_kept_and_others_normalized() {
        assert_eq!(normalized_method("get"), "GET");
        assert_eq!(normalized_method("PATCH"), "PATCH");
        assert_eq!(normalized_method("purge"), "_OTHER");
    }

    #[test]
    fn breaker_states_map_to_the_documented_gauge_values() {
        // 0 = closed / available, 1 = open / blocking.
        assert_eq!(BreakerState::Closed.as_str(), "closed");
        assert_eq!(BreakerState::Open.as_str(), "open");
    }
}
