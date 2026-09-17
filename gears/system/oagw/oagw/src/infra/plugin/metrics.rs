//! Metrics identifier — **catalog identifier only**
//! (`gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1`).
//!
//! Prometheus metrics are *core Data Plane instrumentation* (`infra/metrics.rs`),
//! not a `TransformPlugin` trait implementation. The identifier exists for
//! types-registry cataloging only and is deliberately **not** resolvable via
//! [`TransformPluginRegistry`](crate::infra::plugin::TransformPluginRegistry).
//!
//! Label vocabulary matches the inbound API Gateway so both share dashboards
//! (DESIGN.md "observability"): `http.route` carries the normalised route
//! pattern, `http.request.method` a standard verb, `http.response.status_code`
//! the numeric upstream status, and `host` the upstream alias.

/// GTS instance id of the catalog-only metrics transform.
pub use crate::domain::gts_helpers::METRICS_TRANSFORM_PLUGIN_ID;

/// Metric names the data plane exports.
pub mod names {
    /// Count of proxied requests by outcome.
    pub const REQUESTS_TOTAL: &str = "oagw_requests_total";
    /// Duration histogram of proxied requests, in seconds.
    pub const REQUEST_DURATION_SECONDS: &str = "oagw_request_duration_seconds";
    /// Currently in-flight proxied requests.
    pub const INFLIGHT: &str = "oagw_inflight_requests";
    /// Count of requests rejected before the upstream was called.
    pub const REJECTED_TOTAL: &str = "oagw_rejected_total";
}

/// Metric label names (shared with the inbound API Gateway).
pub mod labels {
    /// Normalised route match pattern (never the raw request path).
    pub const ROUTE: &str = "http.route";
    /// Standard HTTP verb (or `_OTHER`).
    pub const METHOD: &str = "http.request.method";
    /// Numeric upstream status code.
    pub const STATUS_CODE: &str = "http.response.status_code";
    /// Upstream alias.
    pub const HOST: &str = "host";
    /// Upstream GTS instance id.
    pub const UPSTREAM_ID: &str = "oagw.upstream_id";
    /// Which side produced an error.
    pub const ERROR_SOURCE: &str = "oagw.error_source";
}

/// Normalise an HTTP method to a metric label value.
#[must_use]
pub fn normalize_method(method: &str) -> &'static str {
    match method.to_ascii_uppercase().as_str() {
        "GET" => "GET",
        "POST" => "POST",
        "PUT" => "PUT",
        "DELETE" => "DELETE",
        "PATCH" => "PATCH",
        "HEAD" => "HEAD",
        "OPTIONS" => "OPTIONS",
        _ => "_OTHER",
    }
}

/// The `http.route` label value for a matched route.
#[must_use]
pub fn route_label(match_path: &str) -> String {
    match_path.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::plugin::registry::TransformPluginRegistry;

    #[test]
    fn identifier_is_catalog_only() {
        assert!(
            TransformPluginRegistry::with_builtins()
                .get(METRICS_TRANSFORM_PLUGIN_ID)
                .is_none()
        );
    }

    #[test]
    fn unknown_methods_become_other() {
        assert_eq!(normalize_method("get"), "GET");
        assert_eq!(normalize_method("TRACE"), "_OTHER");
    }
}
