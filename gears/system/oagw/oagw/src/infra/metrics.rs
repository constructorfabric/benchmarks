//! Data-plane metrics (DESIGN "Observability").
//!
//! Counters are kept in-process: cheap to read from the management API and
//! free of any dependency on an OpenTelemetry collector being present.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Counters exposed by the gear.
#[derive(Debug, Default)]
pub struct DataPlaneMetrics {
    requests_total: AtomicU64,
    errors_total: AtomicU64,
    rate_limited_total: AtomicU64,
    upstream_2xx: AtomicU64,
    upstream_4xx: AtomicU64,
    upstream_5xx: AtomicU64,
    upgrades_total: AtomicU64,
}

impl DataPlaneMetrics {
    /// New zeroed counter set.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Counts a completed request with its upstream status.
    pub fn record_response(&self, status: http::StatusCode) {
        self.requests_total.fetch_add(1, Ordering::Relaxed);
        match status.as_u16() {
            200..=399 => self.upstream_2xx.fetch_add(1, Ordering::Relaxed),
            400..=499 => self.upstream_4xx.fetch_add(1, Ordering::Relaxed),
            _ => self.upstream_5xx.fetch_add(1, Ordering::Relaxed),
        };
    }

    /// Counts a gateway-generated error.
    pub fn record_error(&self) {
        self.errors_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Counts a 429 rejection.
    pub fn record_rate_limited(&self) {
        self.rate_limited_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Counts a completed WebSocket upgrade.
    pub fn record_upgrade(&self) {
        self.upgrades_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Requests that reached an upstream.
    #[must_use]
    pub fn requests_total(&self) -> u64 {
        self.requests_total.load(Ordering::Relaxed)
    }

    /// Gateway-generated errors.
    #[must_use]
    pub fn errors_total(&self) -> u64 {
        self.errors_total.load(Ordering::Relaxed)
    }

    /// Requests rejected for rate limiting.
    #[must_use]
    pub fn rate_limited_total(&self) -> u64 {
        self.rate_limited_total.load(Ordering::Relaxed)
    }

    /// Completed WebSocket upgrades.
    #[must_use]
    pub fn upgrades_total(&self) -> u64 {
        self.upgrades_total.load(Ordering::Relaxed)
    }

    /// Snapshot of every counter, ready for JSON.
    #[must_use]
    pub fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "requests_total": self.requests_total.load(Ordering::Relaxed),
            "errors_total": self.errors_total.load(Ordering::Relaxed),
            "rate_limited_total": self.rate_limited_total.load(Ordering::Relaxed),
            "upstream_2xx": self.upstream_2xx.load(Ordering::Relaxed),
            "upstream_4xx": self.upstream_4xx.load(Ordering::Relaxed),
            "upstream_5xx": self.upstream_5xx.load(Ordering::Relaxed),
            "upgrades_total": self.upgrades_total.load(Ordering::Relaxed),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_accumulate_by_status_class() {
        let metrics = DataPlaneMetrics::new();
        metrics.record_response(http::StatusCode::OK);
        metrics.record_response(http::StatusCode::NOT_FOUND);
        metrics.record_response(http::StatusCode::BAD_GATEWAY);
        metrics.record_error();
        metrics.record_rate_limited();
        metrics.record_upgrade();
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot["requests_total"], 3);
        assert_eq!(snapshot["upstream_2xx"], 1);
        assert_eq!(snapshot["upstream_4xx"], 1);
        assert_eq!(snapshot["upstream_5xx"], 1);
        assert_eq!(snapshot["errors_total"], 1);
        assert_eq!(snapshot["rate_limited_total"], 1);
        assert_eq!(metrics.requests_total(), 3);
        assert_eq!(metrics.rate_limited_total(), 1);
        assert_eq!(metrics.upgrades_total(), 1);
    }
}
