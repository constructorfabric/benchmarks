// Created: 2026-08-29 by Constructor Tech
//! Observability port of the data plane (DESIGN §4.2).
//!
//! The domain knows *when* something worth measuring happens; the adapter
//! decides *how* it is exported. Keeping the instruments behind a port is what
//! lets the data plane stay free of a metering dependency.

/// How an endpoint was chosen for a request (DESIGN §4.2 "Routing Metrics").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMethod {
    /// The caller named the endpoint with `X-OAGW-Target-Host`.
    ExplicitHeader,
    /// Endpoints were rotated.
    RoundRobin,
    /// The upstream has a single endpoint.
    Default,
}

impl SelectionMethod {
    /// Label value of the selection method.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExplicitHeader => "explicit_header",
            Self::RoundRobin => "round_robin",
            Self::Default => "default",
        }
    }
}

/// Circuit-breaker states reported on a transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    /// Traffic is allowed.
    Closed,
    /// Traffic is refused.
    Open,
}

impl BreakerState {
    /// Label value of the state.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::Open => "open",
        }
    }
}

/// Observability port (DESIGN §4.2).
///
/// Label cardinality follows the design's cardinality rules: no tenant labels,
/// `http.route` carries the normalized route match pattern and `host` carries
/// the upstream alias.
pub trait OagwMetricsPort: Send + Sync {
    /// One completed proxied request, success or failure.
    fn record_request(
        &self,
        host: &str,
        route: &str,
        method: &str,
        status_code: u16,
        duration_seconds: f64,
    );

    /// A request that the gateway rejected with an error type.
    fn record_error(&self, host: &str, route: &str, error_type: &str);

    /// A request rejected because its budget was exhausted.
    fn record_rate_limit_exceeded(&self, host: &str, path: &str);

    /// A circuit-breaker transition for an upstream endpoint.
    fn record_breaker_transition(&self, host: &str, from: BreakerState, to: BreakerState);

    /// The blocking state of an upstream endpoint's breaker.
    fn set_breaker_state(&self, host: &str, state: BreakerState);

    /// The caller selected a specific endpoint with `X-OAGW-Target-Host`.
    fn record_target_host_used(&self, upstream_id: &str, endpoint_host: &str);

    /// An endpoint was chosen for a request.
    fn record_endpoint_selected(
        &self,
        upstream_id: &str,
        endpoint_host: &str,
        method: SelectionMethod,
    );
}
