//! Data-plane instrumentation (DESIGN "observability").
//!
//! Counters are plain atomics keyed by a small label tuple so the proxy hot
//! path stays lock-free; a Prometheus/OpenTelemetry exporter can read them
//! through [`snapshot`]. Label vocabulary comes from
//! [`crate::infra::plugin::metrics`], shared with the inbound API Gateway.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use dashmap::DashMap;

use crate::infra::plugin::metrics::{labels, normalize_method};

/// A single measurement key (route, method, status class, error source).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LabelSet {
    /// `http.route` (the matched route path, never the raw request path).
    pub route: String,
    /// `http.request.method`.
    pub method: &'static str,
    /// `http.response.status_code` (class granularity: `2xx`, `4xx`, `5xx`).
    pub status: String,
    /// `oagw.error_source`.
    pub error_source: &'static str,
}

/// Counters of the data plane.
#[derive(Debug, Default)]
pub struct DpMetrics {
    /// Total proxied requests.
    pub requests_total: AtomicU64,
    /// Requests rejected before the upstream was called.
    pub rejected_total: AtomicU64,
    /// Currently in-flight requests.
    pub inflight: AtomicU64,
    /// Transport failures recorded by the circuit breaker.
    pub transport_failures_total: AtomicU64,
    /// Per-label request counts.
    series: DashMap<LabelSet, AtomicU64>,
}

impl DpMetrics {
    /// Empty counters.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Record the start of a proxied request.
    pub fn begin(&self) {
        self.inflight.fetch_add(1, Ordering::Relaxed);
        self.requests_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Record the end of a proxied request.
    pub fn end(&self) {
        self.inflight.fetch_sub(1, Ordering::Relaxed);
    }

    /// Record a rejected request (never reached the upstream).
    pub fn rejected(&self) {
        self.rejected_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a transport failure.
    pub fn transport_failure(&self) {
        self.transport_failures_total
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Record one completed exchange under `labels`.
    pub fn record(&self, set: LabelSet) {
        self.series
            .entry(set)
            .or_default()
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Snapshot of the per-label counters (for tests and exporters).
    #[must_use]
    pub fn snapshot(&self) -> Vec<(LabelSet, u64)> {
        self.series
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().load(Ordering::Relaxed)))
            .collect()
    }

    /// Count recorded under a specific label set.
    #[must_use]
    pub fn count(
        &self,
        route: &str,
        method: &str,
        status: &str,
        error_source: &'static str,
    ) -> u64 {
        let set = LabelSet {
            route: route.to_owned(),
            method: normalize_method(method),
            status: status.to_owned(),
            error_source,
        };
        self.series
            .get(&set)
            .map(|value| value.load(Ordering::Relaxed))
            .unwrap_or_default()
    }
}

/// Status class label for a response status.
#[must_use]
pub fn status_class(status: u16) -> String {
    match status {
        200..=299 => "2xx".to_owned(),
        300..=399 => "3xx".to_owned(),
        400..=499 => "4xx".to_owned(),
        _ => "5xx".to_owned(),
    }
}

/// Measure an async block and log its duration at `info` level.
pub async fn timed<T>(
    metrics: &DpMetrics,
    labels: LabelSet,
    started: Instant,
    future: impl std::future::Future<Output = T>,
) -> T {
    let value = future.await;
    let elapsed = started.elapsed();
    let route = labels.route.clone();
    metrics.record(labels);
    tracing::info!(target: "oagw::metrics", route = %route, duration_ms = elapsed.as_millis() as u64, "oagw_request_duration_seconds");
    value
}

/// The canonical label set for a completed request.
#[must_use]
pub fn label_set(route: &str, method: &str, status: u16, error_source: &'static str) -> LabelSet {
    LabelSet {
        route: route.to_owned(),
        method: normalize_method(method),
        status: status_class(status),
        error_source,
    }
}

/// The `host` label value (the upstream alias).
#[must_use]
pub fn host_label(alias: &str) -> String {
    alias.to_owned()
}

/// Re-exported label names for callers building their own series.
pub use crate::infra::plugin::metrics::names;

/// Number of label names the DP exports (kept for parity assertions).
#[must_use]
pub const fn label_count() -> usize {
    6
}

/// Alias of [`labels::ROUTE`] for callers that want a shorter path.
pub const ROUTE_LABEL: &str = labels::ROUTE;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_by_label() {
        let metrics = DpMetrics::new();
        metrics.begin();
        assert_eq!(metrics.inflight.load(Ordering::Relaxed), 1);
        metrics.end();
        assert_eq!(metrics.inflight.load(Ordering::Relaxed), 0);
        metrics.record(label_set("/v1", "GET", 200, "upstream"));
        metrics.record(label_set("/v1", "GET", 200, "upstream"));
        assert_eq!(metrics.count("/v1", "GET", "2xx", "upstream"), 2);
        assert_eq!(metrics.count("/v1", "POST", "2xx", "upstream"), 0);
    }

    #[test]
    fn rejected_requests_are_counted_separately() {
        let metrics = DpMetrics::new();
        metrics.rejected();
        assert_eq!(metrics.rejected_total.load(Ordering::Relaxed), 1);
    }
}
