//! Observability: the structured audit record every proxied request emits.
//!
//! A record carries only what ADR 0001 Appendix A fixes: identity, target,
//! outcome and duration. Header values, query strings, credential material and
//! bodies are never included, because the record is written verbatim to the
//! log and the log is not a place where secrets survive review.

use crate::domain::identifiers::now_rfc3339;

/// The fields ADR 0001 Appendix A fixes for a request record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestRecord {
    /// RFC 3339 timestamp of the record.
    pub timestamp: String,
    /// Event name, `proxy_request` or `proxy_error`.
    pub event: &'static str,
    /// Correlation identifier.
    pub request_id: String,
    /// Owning tenant.
    pub tenant_id: String,
    /// Rate-limit or credential principal, already digested.
    pub principal_id: String,
    /// Selected upstream host.
    pub host: String,
    /// Upstream path, without the query string.
    pub path: String,
    /// Request method.
    pub method: String,
    /// Response status, when one was produced.
    pub status: Option<u16>,
    /// Wall-clock duration of the request.
    pub duration_ms: u64,
    /// Request body size in bytes.
    pub request_size: usize,
    /// Response body size in bytes, when known.
    pub response_size: Option<usize>,
    /// GTS error type identifier, when the request failed.
    pub error_type: Option<String>,
}

impl RequestRecord {
    /// Opens a record for a request that has just begun.
    #[must_use]
    pub fn begin(event: &'static str, request_id: &str, tenant_id: &str, method: &str) -> Self {
        Self {
            timestamp: now_rfc3339(),
            event,
            request_id: request_id.to_owned(),
            tenant_id: tenant_id.to_owned(),
            principal_id: String::new(),
            host: String::new(),
            path: String::new(),
            method: method.to_owned(),
            status: None,
            duration_ms: 0,
            request_size: 0,
            response_size: None,
            error_type: None,
        }
    }

    /// Names the upstream this request was resolved to.
    pub fn target(&mut self, host: &str, path: &str, principal: &str) {
        host.clone_into(&mut self.host);
        path.clone_into(&mut self.path);
        principal.clone_into(&mut self.principal_id);
    }

    /// Records the outcome of a request.
    pub fn finish(&mut self, status: Option<u16>, duration: std::time::Duration) {
        self.status = status;
        self.duration_ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
    }

    /// Records a failure's error type.
    pub fn failed(&mut self, error_type: &str) {
        self.error_type = Some(error_type.to_owned());
        self.event = "proxy_error";
    }

    /// Renders the record as one JSON object, ready for the log.
    #[must_use]
    pub fn to_json(&self) -> String {
        let status = self
            .status
            .map_or_else(|| "null".to_owned(), |value| value.to_string());
        let response_size = self
            .response_size
            .map_or_else(|| "null".to_owned(), |value| value.to_string());
        let error_type = self
            .error_type
            .as_deref()
            .map_or_else(|| "null".to_owned(), |value| format!("\"{value}\""));
        format!(
            "{{\"timestamp\":\"{ts}\",\"event\":\"{event}\",\"request_id\":\"{rid}\",\
             \"tenant_id\":\"{tenant}\",\"principal_id\":\"{principal}\",\"host\":\"{host}\",\
             \"path\":\"{path}\",\"method\":\"{method}\",\"status\":{status},\
             \"duration_ms\":{duration},\"request_size\":{request},\
             \"response_size\":{response},\"error_type\":{error}}}",
            ts = self.timestamp,
            event = self.event,
            rid = self.request_id,
            tenant = self.tenant_id,
            principal = self.principal_id,
            host = self.host,
            path = self.path,
            method = self.method,
            status = status,
            duration = self.duration_ms,
            request = self.request_size,
            response = response_size,
            error = error_type,
        )
    }

    /// Writes the record through the `tracing` pipeline.
    pub fn emit(&self) {
        tracing::info!(target: "oagw::audit", record = %self.to_json());
    }
}

/// Counters the gear keeps for itself, cheap enough to update per request.
#[derive(Debug, Default)]
pub struct Metrics {
    /// Requests that reached the proxy.
    pub proxy_requests: std::sync::atomic::AtomicU64,
    /// Requests rejected before the upstream call.
    pub rejected_requests: std::sync::atomic::AtomicU64,
    /// Requests that produced an upstream status.
    pub upstream_responses: std::sync::atomic::AtomicU64,
    /// Responses with a `4xx` or `5xx` upstream status.
    pub upstream_errors: std::sync::atomic::AtomicU64,
    /// Upgraded tunnels opened.
    pub websocket_tunnels: std::sync::atomic::AtomicU64,
    /// Streams relayed end to end.
    pub streams_relayed: std::sync::atomic::AtomicU64,
    /// Streams the upstream cut short.
    pub streams_aborted: std::sync::atomic::AtomicU64,
}

impl Metrics {
    /// An empty counter set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Counts a request entering the proxy.
    pub fn proxy_started(&self) {
        self.proxy_requests
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Counts a request rejected before the upstream call.
    pub fn rejected(&self, _reason: &str) {
        self.rejected_requests
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Counts an upstream answer.
    pub fn upstream_status(&self, status: u16) {
        self.upstream_responses
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if status >= 400 {
            self.upstream_errors
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Counts an upgraded tunnel.
    pub fn tunnel_opened(&self) {
        self.websocket_tunnels
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Counts a relayed stream.
    pub fn stream_relayed(&self) {
        self.streams_relayed
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Counts a stream the upstream cut short.
    pub fn stream_aborted(&self) {
        self.streams_aborted
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// A snapshot of every counter, for the tests and the diagnostics route.
    #[must_use]
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            proxy_requests: self
                .proxy_requests
                .load(std::sync::atomic::Ordering::Relaxed),
            rejected_requests: self
                .rejected_requests
                .load(std::sync::atomic::Ordering::Relaxed),
            upstream_responses: self
                .upstream_responses
                .load(std::sync::atomic::Ordering::Relaxed),
            upstream_errors: self
                .upstream_errors
                .load(std::sync::atomic::Ordering::Relaxed),
            websocket_tunnels: self
                .websocket_tunnels
                .load(std::sync::atomic::Ordering::Relaxed),
            streams_relayed: self
                .streams_relayed
                .load(std::sync::atomic::Ordering::Relaxed),
            streams_aborted: self
                .streams_aborted
                .load(std::sync::atomic::Ordering::Relaxed),
        }
    }
}

/// A point-in-time view of the gear's counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MetricsSnapshot {
    /// Requests that reached the proxy.
    pub proxy_requests: u64,
    /// Requests rejected before the upstream call.
    pub rejected_requests: u64,
    /// Requests that produced an upstream status.
    pub upstream_responses: u64,
    /// Responses with a `4xx` or `5xx` upstream status.
    pub upstream_errors: u64,
    /// Upgraded tunnels opened.
    pub websocket_tunnels: u64,
    /// Streams relayed end to end.
    pub streams_relayed: u64,
    /// Streams the upstream cut short.
    pub streams_aborted: u64,
}
