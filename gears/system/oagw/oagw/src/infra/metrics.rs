// Created: 2026-08-29 by Constructor Tech
//! Data-plane metrics (DESIGN §4.2).
//!
//! Instruments are built once against the global meter provider, so they bind
//! to the real SDK when the platform bootstrapped one and degrade to no-ops
//! when it did not. Label vocabulary follows the OTel HTTP semantic
//! conventions the inbound API Gateway uses, minus any tenant label.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Gauge, Histogram};

/// Request-duration histogram buckets in seconds (DESIGN §4.2).
pub const DURATION_BUCKETS: [f64; 12] = [
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// The OAGW instrument set.
#[derive(Clone)]
pub struct OagwMetrics {
    requests_total: Counter<u64>,
    request_duration: Histogram<f64>,
    requests_in_flight: Gauge<u64>,
    /// Live per-host in-flight counters backing [`OagwMetrics::enter`].
    in_flight: Arc<Mutex<HashMap<String, Arc<AtomicI64>>>>,
    errors_total: Counter<u64>,
    rate_limit_exceeded_total: Counter<u64>,
    routing_endpoint_selected: Counter<u64>,
}

impl std::fmt::Debug for OagwMetrics {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("OagwMetrics")
    }
}

impl Default for OagwMetrics {
    fn default() -> Self {
        Self::build(&opentelemetry::global::meter_with_scope(
            opentelemetry::InstrumentationScope::builder("oagw").build(),
        ))
    }
}

impl OagwMetrics {
    /// Builds the instrument set from a meter.
    #[must_use]
    pub fn build(meter: &opentelemetry::metrics::Meter) -> Self {
        Self {
            requests_total: meter
                .u64_counter("oagw_requests_total")
                .with_description("Proxied requests by upstream, method, route and status")
                .build(),
            request_duration: meter
                .f64_histogram("oagw_request_duration_seconds")
                .with_boundaries(DURATION_BUCKETS.to_vec())
                .with_description("End-to-end request duration by phase")
                .build(),
            requests_in_flight: meter
                .u64_gauge("oagw_requests_in_flight")
                .with_description("Requests currently being proxied")
                .build(),
            in_flight: Arc::new(Mutex::new(HashMap::new())),
            errors_total: meter
                .u64_counter("oagw_errors_total")
                .with_description("Failed proxy hops by error catalog type")
                .build(),
            rate_limit_exceeded_total: meter
                .u64_counter("oagw_rate_limit_exceeded_total")
                .with_description("Requests rejected by the token bucket")
                .build(),
            routing_endpoint_selected: meter
                .u64_counter("oagw_routing_endpoint_selected")
                .with_description("Endpoint selections by method (explicit header or default)")
                .build(),
        }
    }

    /// Records one completed proxy hop.
    pub fn record_request(
        &self,
        host: &str,
        method: &str,
        route: &str,
        status: u16,
        duration_seconds: f64,
    ) {
        let common = self.common(host, method, route);
        self.requests_total.add(
            1,
            &[
                common.0.clone(),
                common.1.clone(),
                common.2.clone(),
                KeyValue::new("http.response.status_code", i64::from(status)),
            ],
        );
        self.request_duration.record(
            duration_seconds,
            &[
                common.0.clone(),
                common.1.clone(),
                KeyValue::new("phase", "proxy"),
            ],
        );
        if status >= 500 {
            self.errors_total.add(
                1,
                &[
                    common.0.clone(),
                    common.1.clone(),
                    KeyValue::new("error_type", "upstream_status"),
                ],
            );
        }
    }

    /// Records a failed hop whose error never reached the upstream.
    ///
    /// The method label is deliberately absent: the error is attributed to the
    /// route, and keeping the label set identical to [`OagwMetrics::enter`]'s
    /// prevents two series for one failure.
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

    /// Opens an in-flight window for `host`, keeping a real per-host counter.
    ///
    /// The gauge is recorded on entry and on exit so a scrape between hops sees
    /// the live count rather than the last completed one; a request that unwinds
    /// through the error path still decrements, because the guard owns it.
    pub fn enter(&self, host: &str) -> InFlightGuard<'_> {
        let counter = {
            let mut counters = self
                .in_flight
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            counters
                .entry(host.to_owned())
                .or_insert_with(|| Arc::new(AtomicI64::new(0)))
                .clone()
        };
        let current = counter.fetch_add(1, Ordering::AcqRel) + 1;
        self.requests_in_flight.record(
            current.max(0) as u64,
            &[KeyValue::new("host", host.to_owned())],
        );
        InFlightGuard {
            metrics: self,
            host: host.to_owned(),
            counter,
        }
    }

    /// Records the current in-flight count.
    pub fn in_flight(&self, host: &str, current: u64) {
        self.requests_in_flight
            .record(current, &[KeyValue::new("host", host.to_owned())]);
    }

    /// Records a rate-limit rejection.
    pub fn record_rate_limit(&self, host: &str, route: &str) {
        self.rate_limit_exceeded_total.add(
            1,
            &[
                KeyValue::new("host", host.to_owned()),
                KeyValue::new("http.route", route.to_owned()),
            ],
        );
    }

    /// Records which endpoint a routing decision landed on.
    pub fn record_endpoint_selection(
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

    /// Normalises an HTTP method to the OTel convention (`_OTHER` for verbs
    /// outside the standard set) so the label cardinality stays bounded.
    #[must_use]
    pub fn normalized_method(method: &str) -> String {
        match method {
            "GET" | "HEAD" | "POST" | "PUT" | "DELETE" | "CONNECT" | "OPTIONS" | "TRACE"
            | "PATCH" => method.to_owned(),
            _ => "_OTHER".to_owned(),
        }
    }

    fn common(&self, host: &str, method: &str, route: &str) -> (KeyValue, KeyValue, KeyValue) {
        (
            KeyValue::new("host", host.to_owned()),
            KeyValue::new("http.request.method", Self::normalized_method(method)),
            KeyValue::new("http.route", route.to_owned()),
        )
    }
}

/// Decrements the per-host in-flight counter when it leaves scope.
#[must_use]
pub struct InFlightGuard<'a> {
    metrics: &'a OagwMetrics,
    host: String,
    counter: Arc<AtomicI64>,
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        let current = self.counter.load(Ordering::Acquire) - 1;
        self.counter.fetch_sub(1, Ordering::AcqRel);
        self.metrics.requests_in_flight.record(
            current.max(0) as u64,
            &[KeyValue::new("host", self.host.clone())],
        );
    }
}

/// Shared metrics handle.
pub type SharedMetrics = Arc<OagwMetrics>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn methods_are_normalized_to_the_otel_vocabulary() {
        for verb in ["GET", "POST", "PATCH"] {
            assert_eq!(OagwMetrics::normalized_method(verb), verb);
        }
        assert_eq!(OagwMetrics::normalized_method("purge"), "_OTHER");
        assert_eq!(OagwMetrics::normalized_method(""), "_OTHER");
    }

    #[test]
    fn buckets_cover_the_documented_range() {
        assert_eq!(DURATION_BUCKETS.first(), Some(&0.001));
        assert_eq!(DURATION_BUCKETS.last(), Some(&10.0));
        assert!(
            DURATION_BUCKETS.windows(2).all(|pair| pair[0] < pair[1]),
            "buckets must be strictly increasing"
        );
    }

    #[test]
    fn instruments_bind_without_a_provider_and_do_not_panic() {
        let metrics = OagwMetrics::default();
        metrics.record_request("api.example:8080", "POST", "/v1/chat", 200, 0.02);
        metrics.record_error("api.example:8080", "/v1/chat", "validation");
        drop(metrics.enter("api.example:8080"));
        drop(metrics.enter("api.example:8080"));
        metrics.record_rate_limit("api.example:8080", "/v1/chat");
        metrics.record_endpoint_selection("u1", "api.example", "default");
    }
}
