// Created: 2026-08-29 by Constructor Tech
//! Audit logging (DESIGN §4.3).
//!
//! OAGW writes structured JSON audit records to stdout through `tracing`, so
//! the platform's log pipeline picks them up like any other gear. Records never
//! carry request bodies, query parameters, headers or credential material.

use serde_json::Value;
use tracing::field::Empty;
use tracing::{info, info_span};

/// Event names emitted by the OAGW audit log.
pub mod event {
    /// A proxied request completed.
    pub const PROXY_REQUEST: &str = "oagw.proxy.request";
    /// A management resource was created, replaced or deleted.
    pub const CONFIG_CHANGE: &str = "oagw.config.change";
    /// An upstream authentication attempt failed.
    pub const AUTH_FAILURE: &str = "oagw.auth.failure";
}

/// Everything the audit log needs about one proxied request.
#[derive(Debug, Clone, Default)]
pub struct ProxyAudit<'a> {
    /// Correlation identifier.
    pub request_id: &'a str,
    /// Owning tenant of the resolved upstream.
    pub tenant_id: &'a str,
    /// Authenticated caller, when the platform resolved one.
    pub principal_id: Option<&'a str>,
    /// Upstream alias the request was routed through.
    pub host: &'a str,
    /// Upstream-facing request path (not the caller's raw path).
    pub path: &'a str,
    /// Request method.
    pub method: &'a str,
    /// Upstream status, or `0` when the gateway itself failed.
    pub status: u16,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
    /// Buffered request bytes.
    pub request_size: usize,
    /// Buffered response bytes.
    pub response_size: usize,
    /// Error catalog type, when the hop failed.
    pub error_type: Option<&'a str>,
}

/// Emits one `oagw.proxy.request` audit record.
///
/// No PII: the record carries identifiers and sizes only.
pub fn proxy_request(audit: &ProxyAudit<'_>) {
    let span = info_span!(
        "oagw_audit",
        event = Empty,
        request_id = Empty,
        tenant_id = Empty,
        principal_id = Empty,
        host = Empty,
        path = Empty,
        method = Empty,
        status = Empty,
        duration_ms = Empty,
        request_size = Empty,
        response_size = Empty,
        error_type = Empty,
    );
    let _guard = span.enter();
    if audit.error_type.is_some() {
        tracing::warn!(
            target: "oagw::audit",
            event = event::PROXY_REQUEST,
            request_id = audit.request_id,
            tenant_id = audit.tenant_id,
            principal_id = audit.principal_id.unwrap_or(""),
            host = audit.host,
            path = audit.path,
            method = audit.method,
            status = audit.status,
            duration_ms = audit.duration_ms,
            request_size = audit.request_size,
            response_size = audit.response_size,
            error_type = audit.error_type.unwrap_or(""),
            "oagw proxy request failed"
        );
        return;
    }
    info!(
        target: "oagw::audit",
        event = event::PROXY_REQUEST,
        request_id = audit.request_id,
        tenant_id = audit.tenant_id,
        principal_id = audit.principal_id.unwrap_or(""),
        host = audit.host,
        path = audit.path,
        method = audit.method,
        status = audit.status,
        duration_ms = audit.duration_ms,
        request_size = audit.request_size,
        response_size = audit.response_size,
        "oagw proxy request"
    );
}

/// Emits one `oagw.config.change` audit record for a management write.
pub fn config_change(resource: &str, action: &str, tenant_id: &str, id: &str) {
    info!(
        target: "oagw::audit",
        event = event::CONFIG_CHANGE,
        resource,
        action,
        tenant_id,
        id,
        "oagw configuration changed"
    );
}

/// Emits a rate-limited `oagw.auth.failure` record (DESIGN §4.3 warns against
/// flooding the log, so the caller passes its sampling decision).
pub fn auth_failure(tenant_id: &str, host: &str, reference: &str, sample: bool) {
    if !sample {
        return;
    }
    tracing::warn!(
        target: "oagw::audit",
        event = event::AUTH_FAILURE,
        tenant_id,
        host,
        plugin = reference,
        "oagw upstream authentication failed"
    );
}

/// `true` when the caller should sample an auth-failure record at `1/100`.
#[must_use]
pub fn sample_auth_failure() -> bool {
    // Cheap, allocation-free sampling: rotate through 100 slots.
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed).is_multiple_of(100)
}

/// Renders an audit record as the JSON shape centralized logging ingests.
///
/// Exposed for tests and for operators who post-process the records.
#[must_use]
pub fn audit_record(audit: &ProxyAudit<'_>) -> Value {
    serde_json::json!({
        "event": event::PROXY_REQUEST,
        "request_id": audit.request_id,
        "tenant_id": audit.tenant_id,
        "principal_id": audit.principal_id,
        "host": audit.host,
        "path": audit.path,
        "method": audit.method,
        "status": audit.status,
        "duration_ms": audit.duration_ms,
        "request_size": audit.request_size,
        "response_size": audit.response_size,
        "error_type": audit.error_type,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_records_carry_no_request_payload() {
        let record = audit_record(&ProxyAudit {
            request_id: "req-1",
            tenant_id: "tenant-1",
            principal_id: Some("user-1"),
            host: "api.example:8080",
            path: "/v1/chat",
            method: "POST",
            status: 200,
            duration_ms: 12,
            request_size: 1_024,
            response_size: 2_048,
            error_type: None,
        });
        assert_eq!(record["event"], event::PROXY_REQUEST);
        assert_eq!(record["status"], 200);
        let rendered = record.to_string();
        for forbidden in ["body", "authorization", "query", "headers"] {
            assert!(!rendered.contains(forbidden), "audit leaks {forbidden}");
        }
    }

    #[test]
    fn auth_failures_are_sampled_and_skippable() {
        // The helper must not panic and must honour the sampling switch.
        auth_failure("tenant", "api.example:8080", "plugin", false);
        auth_failure("tenant", "api.example:8080", "plugin", true);
        let _sampled = sample_auth_failure();
    }
}
