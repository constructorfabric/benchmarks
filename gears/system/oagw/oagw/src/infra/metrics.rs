//! Observable counters for the OAGW data plane.
//!
//! All request handling is observable (constitution principle V): the gateway
//! records request counts by upstream and status class, error counts by kind,
//! rate-limit rejections by scope and `X-OAGW-Target-Host` usage by endpoint.
//! Counters are plain atomics so tests can read them without an exporter.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use parking_lot::RwLock;

/// Counter name for proxied requests.
pub const PROXY_REQUESTS_TOTAL: &str = "oagw_proxy_requests_total";
/// Counter name for proxy errors.
pub const PROXY_ERRORS_TOTAL: &str = "oagw_proxy_errors_total";
/// Counter name for rate-limit rejections.
pub const RATE_LIMIT_REJECTIONS_TOTAL: &str = "oagw_rate_limit_rejections_total";
/// Counter name for `X-OAGW-Target-Host` usage.
pub const ROUTING_TARGET_HOST_USED: &str = "oagw_routing_target_host_used";

#[derive(Default)]
struct Counters {
    requests: AtomicU64,
    errors: AtomicU64,
    rejections: AtomicU64,
    /// Per upstream id: request count.
    per_upstream: RwLock<std::collections::HashMap<String, u64>>,
    /// Per error kind name: count.
    per_error: RwLock<std::collections::HashMap<String, u64>>,
    /// Per endpoint host: `X-OAGW-Target-Host` selections.
    per_target_host: RwLock<std::collections::HashMap<String, u64>>,
    /// Recorded request latencies in microseconds, capped by `record`.
    latencies: RwLock<Vec<u64>>,
}

/// Metrics handle for the gear.
#[derive(Clone, Default)]
pub struct Metrics {
    inner: Arc<Counters>,
}

impl Metrics {
    /// Create an independent metrics handle.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one proxied request.
    pub fn record_request(&self, upstream_id: &str, status_class: &str) {
        self.inner.requests.fetch_add(1, Ordering::Relaxed);
        *self
            .inner
            .per_upstream
            .write()
            .entry(upstream_id.to_owned())
            .or_insert(0) += 1;
        tracing::debug!(
            metric = PROXY_REQUESTS_TOTAL,
            upstream_id,
            status_class,
            "oagw request counted"
        );
    }

    /// Record one error by kind.
    pub fn record_error(&self, kind: &str) {
        self.inner.errors.fetch_add(1, Ordering::Relaxed);
        *self
            .inner
            .per_error
            .write()
            .entry(kind.to_owned())
            .or_insert(0) += 1;
    }

    /// Record one rate-limit rejection for `scope`.
    pub fn record_rejection(&self, scope: &str) {
        self.inner.rejections.fetch_add(1, Ordering::Relaxed);
        *self
            .inner
            .per_error
            .write()
            .entry(format!("rate_limit_rejected:{scope}"))
            .or_insert(0) += 1;
        tracing::debug!(
            metric = RATE_LIMIT_REJECTIONS_TOTAL,
            scope,
            "oagw rate limit rejection counted"
        );
    }

    /// Record one `X-OAGW-Target-Host` selection.
    pub fn record_target_host(&self, endpoint_host: &str) {
        *self
            .inner
            .per_target_host
            .write()
            .entry(endpoint_host.to_owned())
            .or_insert(0) += 1;
    }

    /// Record a request latency.
    pub fn record_latency(&self, started: Instant) {
        let micros = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        let mut latencies = self.inner.latencies.write();
        latencies.push(micros);
        // Keep the window bounded; only recent samples matter for p95.
        if latencies.len() > 1_000 {
            let overflow = latencies.len() - 1_000;
            latencies.drain(0..overflow);
        }
    }

    /// Total proxied requests.
    #[must_use]
    pub fn requests_total(&self) -> u64 {
        self.inner.requests.load(Ordering::Relaxed)
    }

    /// Total recorded errors.
    #[must_use]
    pub fn errors_total(&self) -> u64 {
        self.inner.errors.load(Ordering::Relaxed)
    }

    /// Total rate-limit rejections.
    #[must_use]
    pub fn rejections_total(&self) -> u64 {
        self.inner.rejections.load(Ordering::Relaxed)
    }

    /// Requests recorded for one upstream.
    #[must_use]
    pub fn requests_for_upstream(&self, upstream_id: &str) -> u64 {
        self.inner
            .per_upstream
            .read()
            .get(upstream_id)
            .copied()
            .unwrap_or(0)
    }

    /// Errors recorded for one kind name.
    #[must_use]
    pub fn errors_for_kind(&self, kind: &str) -> u64 {
        self.inner.per_error.read().get(kind).copied().unwrap_or(0)
    }

    /// `X-OAGW-Target-Host` selections for one endpoint host.
    #[must_use]
    pub fn target_host_selections(&self, endpoint_host: &str) -> u64 {
        self.inner
            .per_target_host
            .read()
            .get(endpoint_host)
            .copied()
            .unwrap_or(0)
    }

    /// The p95 latency in microseconds over the recorded window.
    #[must_use]
    pub fn latency_p95_micros(&self) -> Option<u64> {
        let mut latencies = self.inner.latencies.read().clone();
        if latencies.is_empty() {
            return None;
        }
        latencies.sort_unstable();
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let index = (latencies.len() as f64 * 0.95).ceil() as usize;
        Some(latencies[index.saturating_sub(1)])
    }

    /// Prometheus-style exposition of the counters.
    #[must_use]
    pub fn render(&self) -> String {
        let counters = [
            (PROXY_REQUESTS_TOTAL, self.requests_total()),
            (PROXY_ERRORS_TOTAL, self.errors_total()),
            (RATE_LIMIT_REJECTIONS_TOTAL, self.rejections_total()),
        ];
        let lines: Vec<String> = counters
            .iter()
            .map(|(name, value)| format!("# TYPE {name} counter\n{name} {value}"))
            .collect();
        lines.join("\n") + "\n"
    }
}

/// Process-global metrics handle, shared by every data plane instance.
pub fn global() -> Metrics {
    static GLOBAL: std::sync::OnceLock<Metrics> = std::sync::OnceLock::new();
    GLOBAL.get_or_init(Metrics::new).clone()
}

#[cfg(test)]
mod metrics_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn counters_accumulate_per_dimension() {
        let m = Metrics::new();
        m.record_request("u1", "2xx");
        m.record_request("u1", "2xx");
        m.record_request("u2", "4xx");
        assert_eq!(m.requests_total(), 3);
        assert_eq!(m.requests_for_upstream("u1"), 2);
        assert_eq!(m.requests_for_upstream("u2"), 1);
        assert_eq!(m.requests_for_upstream("u3"), 0);
    }

    #[test]
    fn errors_and_rejections_are_counted_by_name() {
        let m = Metrics::new();
        m.record_error("downstream");
        m.record_error("downstream");
        m.record_rejection("tenant");
        assert_eq!(m.errors_total(), 2);
        assert_eq!(m.errors_for_kind("downstream"), 2);
        assert_eq!(m.errors_for_kind("other"), 0);
        assert_eq!(m.errors_for_kind("rate_limit_rejected:tenant"), 1);
        assert_eq!(m.rejections_total(), 1);
    }

    #[test]
    fn target_host_and_latency_are_recorded() {
        let m = Metrics::new();
        m.record_target_host("us.vendor.com");
        m.record_target_host("us.vendor.com");
        assert_eq!(m.target_host_selections("us.vendor.com"), 2);

        let m2 = Metrics::new();
        assert_eq!(m2.latency_p95_micros(), None);
        for _ in 0..100 {
            // One microsecond ago; `checked_sub` covers a clock at zero.
            let at = std::time::Duration::from_micros(1);
            let stamp = Instant::now().checked_sub(at).unwrap_or(Instant::now());
            m2.record_latency(stamp);
        }
        assert!(m2.latency_p95_micros().is_some());
    }

    #[test]
    fn render_exposes_counters() {
        let m = global();
        let rendered = m.render();
        assert!(rendered.contains(PROXY_REQUESTS_TOTAL));
        assert!(rendered.contains(PROXY_ERRORS_TOTAL));
        assert!(rendered.contains(RATE_LIMIT_REJECTIONS_TOTAL));
    }
}
