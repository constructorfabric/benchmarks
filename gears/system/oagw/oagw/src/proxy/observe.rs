//! Observe the Proxy Request (`cpt-cf-oagw-algo-proxy-observe-request`).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use dashmap::DashMap;

use crate::audit::{AuditLogEntry, AuditLogFields};
use crate::correlation::assign_correlation_id;

/// Bounded-cardinality base-metric registry for the proxy path
/// (`inst-proxy-obs-counters`, `inst-proxy-obs-cardinality`). In-process
/// only; this round does not wire an external metrics exporter, but every
/// increment below is independently observable by this module's own tests,
/// keeping `cpt-cf-oagw-dod-proxy-observability` verifiable rather than
/// aspirational.
#[derive(Debug, Default)]
pub(crate) struct ProxyMetrics {
    requests_total: DashMap<(String, String, String, u16), AtomicU64>,
    errors_total: DashMap<(String, String, String), AtomicU64>,
    in_flight: DashMap<String, AtomicI64>,
    endpoint_selected: DashMap<(String, String, &'static str), AtomicU64>,
}

impl ProxyMetrics {
    pub fn global() -> &'static Self {
        static METRICS: OnceLock<ProxyMetrics> = OnceLock::new();
        METRICS.get_or_init(Self::default)
    }

    /// `oagw_requests_in_flight{host}` increment on entry
    /// (`inst-proxy-obs-in-flight`).
    pub fn enter_in_flight(&self, host: &str) {
        self.in_flight
            .entry(host.to_owned())
            .or_insert_with(|| AtomicI64::new(0))
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Decrement on completion, including on every error path.
    pub fn exit_in_flight(&self, host: &str) {
        if let Some(counter) = self.in_flight.get(host) {
            counter.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Read-side accessor this module's own tests use to verify
    /// [`Self::enter_in_flight`]/[`Self::exit_in_flight`] behaviour.
    #[allow(dead_code)]
    #[must_use]
    pub fn in_flight(&self, host: &str) -> i64 {
        self.in_flight
            .get(host)
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// `oagw_requests_total{host, http.request.method, http.route,
    /// http.response.status_code}` (`inst-proxy-obs-counters`).
    pub fn record_request(&self, host: &str, method: &str, route: &str, status: u16) {
        let key = (host.to_owned(), method.to_owned(), route.to_owned(), status);
        self.requests_total
            .entry(key)
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Read-side accessor this module's own tests use to verify
    /// [`Self::record_request`] behaviour.
    #[allow(dead_code)]
    #[must_use]
    pub fn requests_total(&self, host: &str, method: &str, route: &str, status: u16) -> u64 {
        self.requests_total
            .get(&(host.to_owned(), method.to_owned(), route.to_owned(), status))
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// `oagw_errors_total{host, http.route, error_type}`.
    pub fn record_error(&self, host: &str, route: &str, error_type: &str) {
        let key = (host.to_owned(), route.to_owned(), error_type.to_owned());
        self.errors_total
            .entry(key)
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Read-side accessor this module's own tests use to verify
    /// [`Self::record_error`] behaviour.
    #[allow(dead_code)]
    #[must_use]
    pub fn errors_total(&self, host: &str, route: &str, error_type: &str) -> u64 {
        self.errors_total
            .get(&(host.to_owned(), route.to_owned(), error_type.to_owned()))
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// `oagw_routing_endpoint_selected{upstream_id, endpoint_host,
    /// selection_method}` on every selection (`inst-proxy-ep-metrics`).
    /// Keyed here by the upstream's alias rather than its UUID, matching
    /// this path's bounded-cardinality `host` label convention.
    pub fn record_endpoint_selected(
        &self,
        host: &str,
        endpoint_host: &str,
        method: crate::proxy::endpoint::SelectionMethod,
    ) {
        let key = (host.to_owned(), endpoint_host.to_owned(), method.as_str());
        self.endpoint_selected
            .entry(key)
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Read-side accessor this module's own tests use to verify
    /// [`Self::record_endpoint_selected`] behaviour.
    #[allow(dead_code)]
    #[must_use]
    pub fn endpoint_selected(
        &self,
        host: &str,
        endpoint_host: &str,
        method: crate::proxy::endpoint::SelectionMethod,
    ) -> u64 {
        self.endpoint_selected
            .get(&(host.to_owned(), endpoint_host.to_owned(), method.as_str()))
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or(0)
    }
}

/// Normalize a request method to a standard verb or `_OTHER`
/// (`inst-proxy-obs-cardinality`).
#[must_use]
pub(crate) fn normalize_method(method: &axum::http::Method) -> &'static str {
    match *method {
        axum::http::Method::GET => "GET",
        axum::http::Method::POST => "POST",
        axum::http::Method::PUT => "PUT",
        axum::http::Method::DELETE => "DELETE",
        axum::http::Method::PATCH => "PATCH",
        _ => "_OTHER",
    }
}

/// Fields resolved at request-completion time
/// (`inst-proxy-obs-audit-record`).
#[derive(Debug, Clone, Default)]
pub(crate) struct ObserveOutcome {
    pub tenant_id: Option<String>,
    pub principal_id: Option<String>,
    pub host: Option<String>,
    /// The actual resolved outbound path (may carry a caller-supplied
    /// `path_suffix`) -- used for the audit-log record only. RF-004:
    /// **never** used as the `ProxyMetrics` `route` label; see
    /// [`Self::route_template`] for that.
    pub path: Option<String>,
    /// RF-004: the matched route's fixed `http_match.path` template (never
    /// the guard-resolved, caller-suffix-bearing outbound path) -- the
    /// bounded-cardinality value `ProxyMetrics::record_request`/
    /// `record_error`'s `route` label is keyed on. `None` when no route was
    /// matched at all (falls back to `"unknown"`, a single bounded bucket).
    pub route_template: Option<String>,
    pub method: Option<String>,
    pub status: Option<u16>,
    pub duration_ms: Option<u64>,
    pub error_type: Option<String>,
}

/// `cpt-cf-oagw-algo-proxy-observe-request`: assign/reuse a correlation id,
/// emit exactly one structured audit record and update the base counters.
/// Returns the correlation id so the caller can expose it as `trace_id` in
/// a gateway problem-details body (`inst-proxy-obs-expose`).
// @cpt-algo:cpt-cf-oagw-algo-proxy-observe-request:p2
// @cpt-dod:cpt-cf-oagw-dod-proxy-observability:p2
// @cpt-begin:cpt-cf-oagw-algo-proxy-observe-request:p2:inst-proxy-obs-correlate
pub(crate) fn correlate(headers: &axum::http::HeaderMap) -> String {
    assign_correlation_id(headers)
}
// @cpt-end:cpt-cf-oagw-algo-proxy-observe-request:p2:inst-proxy-obs-correlate

// @cpt-begin:cpt-cf-oagw-algo-proxy-observe-request:p2:inst-proxy-obs-audit-record
// @cpt-begin:cpt-cf-oagw-algo-proxy-observe-request:p2:inst-proxy-obs-levels
// @cpt-begin:cpt-cf-oagw-algo-proxy-observe-request:p2:inst-proxy-obs-no-pii
pub(crate) fn observe_completion(request_id: &str, outcome: &ObserveOutcome) {
    let level = if outcome.error_type.is_some() {
        "ERROR"
    } else {
        "INFO"
    };
    let fields = AuditLogFields {
        tenant_id: outcome.tenant_id.clone(),
        principal_id: outcome.principal_id.clone(),
        host: outcome.host.clone(),
        path: outcome.path.clone(),
        method: outcome.method.clone(),
        status: outcome.status,
        duration_ms: outcome.duration_ms,
    };
    // @cpt-begin:cpt-cf-oagw-algo-proxy-observe-request:p2:inst-proxy-obs-return
    AuditLogEntry::new("proxy_request", level, request_id, fields).emit();
    // @cpt-end:cpt-cf-oagw-algo-proxy-observe-request:p2:inst-proxy-obs-return
    // @cpt-end:cpt-cf-oagw-algo-proxy-observe-request:p2:inst-proxy-obs-no-pii
    // @cpt-end:cpt-cf-oagw-algo-proxy-observe-request:p2:inst-proxy-obs-levels
    // @cpt-end:cpt-cf-oagw-algo-proxy-observe-request:p2:inst-proxy-obs-audit-record

    // @cpt-begin:cpt-cf-oagw-algo-proxy-observe-request:p2:inst-proxy-obs-counters
    // @cpt-begin:cpt-cf-oagw-algo-proxy-observe-request:p2:inst-proxy-obs-cardinality
    let metrics = ProxyMetrics::global();
    let host = outcome.host.as_deref().unwrap_or("unknown");
    // RF-004: the route *template*, never the guard-resolved outbound path
    // (which carries an unbounded, attacker-controlled `path_suffix`
    // whenever `path_suffix_mode: append` -- the schema default). Keying on
    // the template keeps this map's cardinality bounded by the number of
    // configured routes, not the number of distinct paths a caller has ever
    // sent.
    let route = outcome.route_template.as_deref().unwrap_or("unknown");
    let method = outcome.method.as_deref().unwrap_or("_OTHER");
    // @cpt-end:cpt-cf-oagw-algo-proxy-observe-request:p2:inst-proxy-obs-cardinality
    if let Some(status) = outcome.status {
        metrics.record_request(host, method, route, status);
    }
    // @cpt-begin:cpt-cf-oagw-algo-proxy-observe-request:p2:inst-proxy-obs-not-mine
    // No rate-limit, circuit-breaker or upstream-health metric is emitted
    // from this path: those depend on machinery owned by DECOMPOSITION
    // entries 2.8/2.9 or are excluded from this round entirely -- only
    // `record_error` below, for this path's own failures, runs here.
    if let Some(error_type) = &outcome.error_type {
        metrics.record_error(host, route, error_type);
    }
    // @cpt-end:cpt-cf-oagw-algo-proxy-observe-request:p2:inst-proxy-obs-not-mine
    // @cpt-end:cpt-cf-oagw-algo-proxy-observe-request:p2:inst-proxy-obs-counters
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_flight_increments_and_decrements() {
        let metrics = ProxyMetrics::default();
        metrics.enter_in_flight("h");
        metrics.enter_in_flight("h");
        assert_eq!(metrics.in_flight("h"), 2);
        metrics.exit_in_flight("h");
        assert_eq!(metrics.in_flight("h"), 1);
    }

    #[test]
    fn requests_and_errors_totals_increment() {
        let metrics = ProxyMetrics::default();
        metrics.record_request("h", "GET", "/r", 200);
        metrics.record_request("h", "GET", "/r", 200);
        assert_eq!(metrics.requests_total("h", "GET", "/r", 200), 2);

        metrics.record_error("h", "/r", "RouteNotFound");
        assert_eq!(metrics.errors_total("h", "/r", "RouteNotFound"), 1);
    }

    #[test]
    fn normalize_method_maps_standard_verbs() {
        assert_eq!(normalize_method(&axum::http::Method::GET), "GET");
        assert_eq!(normalize_method(&axum::http::Method::TRACE), "_OTHER");
    }

    /// RF-004: `observe_completion` must key `ProxyMetrics` on the route's
    /// *template* (`ObserveOutcome::route_template`), not the guard-resolved
    /// outbound path (`ObserveOutcome::path`) -- two distinct,
    /// caller-controlled path suffixes matched by the same route must
    /// collapse onto a single, bounded metric entry rather than growing the
    /// map without bound.
    #[test]
    fn two_distinct_path_suffixes_on_one_route_collapse_to_a_single_metric_entry() {
        let host = "rf-004-cardinality-host";
        let outcome_a = ObserveOutcome {
            host: Some(host.to_owned()),
            method: Some("GET".to_owned()),
            status: Some(200),
            path: Some("/v1/models/attacker-controlled-suffix-1".to_owned()),
            route_template: Some("/v1/models".to_owned()),
            ..Default::default()
        };
        let outcome_b = ObserveOutcome {
            host: Some(host.to_owned()),
            method: Some("GET".to_owned()),
            status: Some(200),
            path: Some("/v1/models/an-entirely-different-suffix-2".to_owned()),
            route_template: Some("/v1/models".to_owned()),
            ..Default::default()
        };
        observe_completion("req-1", &outcome_a);
        observe_completion("req-2", &outcome_b);

        let metrics = ProxyMetrics::global();
        assert_eq!(
            metrics.requests_total(host, "GET", "/v1/models", 200),
            2,
            "both distinct outbound paths must collapse onto the same route-template bucket"
        );
    }
}
