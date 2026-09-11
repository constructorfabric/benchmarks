//! OpenTelemetry instrumentation for the Data Plane
//! (`cpt-cf-oagw-nfr-observability`).
//!
//! Label keys follow the OTel HTTP semantic conventions so OAGW and the
//! inbound API gateway share dashboards. Cardinality is bounded deliberately:
//! no tenant labels, `http.route` is the matched pattern rather than the raw
//! path, and the method is normalized to a standard verb or `_OTHER`.

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Gauge, Histogram, Meter};

/// Instrumentation scope name.
pub const METER_NAME: &str = "oagw";

const OAGW_REQUESTS: &str = "oagw_requests_total";
const OAGW_REQUEST_DURATION: &str = "oagw_request_duration_seconds";
const OAGW_REQUESTS_IN_FLIGHT: &str = "oagw_requests_in_flight";
const OAGW_ERRORS: &str = "oagw_errors_total";
const OAGW_RATE_LIMIT_EXCEEDED: &str = "oagw_rate_limit_exceeded_total";
const OAGW_RATE_LIMIT_USAGE_RATIO: &str = "oagw_rate_limit_usage_ratio";
const OAGW_ROUTING_TARGET_HOST_USED: &str = "oagw_routing_target_host_used";
const OAGW_ROUTING_ENDPOINT_SELECTED: &str = "oagw_routing_endpoint_selected";

/// The standard verbs; anything else collapses to `_OTHER`.
const STANDARD_METHODS: &[&str] = &[
    "GET", "HEAD", "POST", "PUT", "DELETE", "CONNECT", "OPTIONS", "TRACE", "PATCH",
];

/// Normalize a request method for use as a metric label.
#[must_use]
pub fn normalize_method(method: &str) -> &'static str {
    STANDARD_METHODS
        .iter()
        .find(|candidate| candidate.eq_ignore_ascii_case(method))
        .copied()
        .unwrap_or("_OTHER")
}

/// How an endpoint was picked out of the pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMethod {
    ExplicitHeader,
    RoundRobin,
    Default,
}

impl SelectionMethod {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExplicitHeader => "explicit_header",
            Self::RoundRobin => "round_robin",
            Self::Default => "default",
        }
    }
}

/// Data Plane instrument set.
pub struct OagwMetrics {
    requests: Counter<u64>,
    duration: Histogram<f64>,
    in_flight: Gauge<i64>,
    errors: Counter<u64>,
    rate_limit_exceeded: Counter<u64>,
    rate_limit_usage_ratio: Gauge<f64>,
    target_host_used: Counter<u64>,
    endpoint_selected: Counter<u64>,
}

impl std::fmt::Debug for OagwMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OagwMetrics").finish_non_exhaustive()
    }
}

impl OagwMetrics {
    /// Build instruments from the process-global meter provider.
    #[must_use]
    pub fn from_global() -> Self {
        Self::new(&opentelemetry::global::meter(METER_NAME))
    }

    #[must_use]
    pub fn new(meter: &Meter) -> Self {
        Self {
            requests: meter
                .u64_counter(OAGW_REQUESTS)
                .with_description("Proxy requests by upstream alias, method, route and status")
                .build(),
            duration: meter
                .f64_histogram(OAGW_REQUEST_DURATION)
                .with_description("Proxy request duration in seconds, by phase")
                .build(),
            in_flight: meter
                .i64_gauge(OAGW_REQUESTS_IN_FLIGHT)
                .with_description("Proxy requests currently in flight, by upstream alias")
                .build(),
            errors: meter
                .u64_counter(OAGW_ERRORS)
                .with_description("Gateway-originated errors by type")
                .build(),
            rate_limit_exceeded: meter
                .u64_counter(OAGW_RATE_LIMIT_EXCEEDED)
                .with_description("Requests rejected by a rate limit")
                .build(),
            rate_limit_usage_ratio: meter
                .f64_gauge(OAGW_RATE_LIMIT_USAGE_RATIO)
                .with_description("Fraction of the rate limit budget consumed")
                .build(),
            target_host_used: meter
                .u64_counter(OAGW_ROUTING_TARGET_HOST_USED)
                .with_description("Requests routed by an explicit X-OAGW-Target-Host header")
                .build(),
            endpoint_selected: meter
                .u64_counter(OAGW_ROUTING_ENDPOINT_SELECTED)
                .with_description("Endpoint selections by method")
                .build(),
        }
    }

    /// Record a completed proxy request.
    pub fn record_request(&self, host: &str, method: &str, route: &str, status: u16, secs: f64) {
        let labels = [
            KeyValue::new("host", host.to_owned()),
            KeyValue::new("http.request.method", normalize_method(method)),
            KeyValue::new("http.route", route.to_owned()),
            KeyValue::new("http.response.status_code", i64::from(status)),
        ];
        self.requests.add(1, &labels);
        self.duration.record(
            secs,
            &[
                KeyValue::new("host", host.to_owned()),
                KeyValue::new("http.route", route.to_owned()),
                KeyValue::new("phase", "total"),
            ],
        );
    }

    /// Record a phase duration (`resolve`, `plugins`, `upstream`, …).
    pub fn record_phase(&self, host: &str, route: &str, phase: &'static str, secs: f64) {
        self.duration.record(
            secs,
            &[
                KeyValue::new("host", host.to_owned()),
                KeyValue::new("http.route", route.to_owned()),
                KeyValue::new("phase", phase),
            ],
        );
    }

    pub fn set_in_flight(&self, host: &str, value: i64) {
        self.in_flight
            .record(value, &[KeyValue::new("host", host.to_owned())]);
    }

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

    pub fn record_rate_limit(&self, host: &str, path: &str, exceeded: bool, usage_ratio: f64) {
        let labels = [
            KeyValue::new("host", host.to_owned()),
            KeyValue::new("path", path.to_owned()),
        ];
        if exceeded {
            self.rate_limit_exceeded.add(1, &labels);
        }
        self.rate_limit_usage_ratio.record(usage_ratio, &labels);
    }

    pub fn record_endpoint_selection(
        &self,
        upstream_id: &str,
        endpoint_host: &str,
        selection: SelectionMethod,
    ) {
        if selection == SelectionMethod::ExplicitHeader {
            self.target_host_used.add(
                1,
                &[
                    KeyValue::new("upstream_id", upstream_id.to_owned()),
                    KeyValue::new("endpoint_host", endpoint_host.to_owned()),
                ],
            );
        }
        self.endpoint_selected.add(
            1,
            &[
                KeyValue::new("upstream_id", upstream_id.to_owned()),
                KeyValue::new("endpoint_host", endpoint_host.to_owned()),
                KeyValue::new("selection_method", selection.as_str()),
            ],
        );
    }
}

#[cfg(test)]
#[path = "metrics_tests.rs"]
mod tests;
