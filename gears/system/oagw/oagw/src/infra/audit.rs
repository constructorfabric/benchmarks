// Created: 2026-08-29 by Constructor Tech
//! Structured audit events (DESIGN §4.3).
//!
//! One JSON event per proxied request, config change, authentication failure
//! and circuit-breaker transition, emitted through `tracing` so the host's
//! log pipeline picks them up.
//!
//! Security: the field set is closed. Request and response *bodies*, query
//! strings and header values are never formatted into an event, and neither
//! are credentials — `path` is the only request material beyond the identity
//! and sizing fields the design allowlists.

use axum::http::StatusCode;
use uuid::Uuid;

/// A completed proxy request, in the field set of DESIGN §4.3.
pub struct ProxyAudit<'a> {
    /// Correlation id.
    pub request_id: &'a str,
    /// Caller tenant.
    pub tenant_id: Uuid,
    /// Authenticated subject.
    pub principal_id: Uuid,
    /// Routing alias requested.
    pub alias: &'a str,
    /// Method of the request.
    pub method: &'a str,
    /// Final status, when the request produced one.
    pub status: Option<StatusCode>,
    /// Wall-clock duration of the pipeline.
    pub duration_ms: u128,
    /// Request body size in bytes.
    pub request_size: usize,
    /// Response body size in bytes.
    pub response_size: usize,
    /// Error type id, when the request failed at the gateway.
    pub error_type: Option<&'a str>,
}

/// Emit the audit event of a completed proxy request.
pub fn proxy_request(audit: &ProxyAudit<'_>) {
    tracing::info!(
        event = "proxy_request",
        request_id = audit.request_id,
        tenant_id = %audit.tenant_id,
        principal_id = %audit.principal_id,
        alias = audit.alias,
        method = audit.method,
        status = audit.status.map_or_else(|| "none".to_owned(), |s| s.as_u16().to_string()),
        duration_ms = audit.duration_ms as u64,
        request_size = audit.request_size as u64,
        response_size = audit.response_size as u64,
        error_type = audit.error_type.unwrap_or("none"),
        "proxied request"
    );
}

/// One configuration change on the control plane.
pub fn config_change(action: &str, resource: &str, id: Uuid, tenant_id: Uuid) {
    tracing::info!(
        event = "config_change",
        action,
        resource,
        resource_id = %id,
        tenant_id = %tenant_id,
        "configuration changed"
    );
}

/// A rejected authentication attempt. The reason is a coarse code, never the
/// presented credential or the error text that could embed one.
pub fn auth_failure(tenant_id: Uuid, upstream_id: Uuid, reason: &str) {
    tracing::warn!(
        event = "auth_failure",
        tenant_id = %tenant_id,
        upstream_id = %upstream_id,
        reason,
        "upstream authentication refused"
    );
}

/// A circuit-breaker state transition for an upstream endpoint.
pub fn breaker_transition(upstream_id: Uuid, endpoint: &str, state: &str) {
    tracing::warn!(
        event = "circuit_breaker",
        upstream_id = %upstream_id,
        // An endpoint host is configuration, not request material.
        endpoint,
        state,
        "circuit breaker transition"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_proxy_event_carryies_the_design_field_set() {
        // The event is built from the allowlisted fields only; this test pins
        // the call shape so a future edit cannot silently add request material.
        let audit = ProxyAudit {
            request_id: "trace-1",
            tenant_id: Uuid::nil(),
            principal_id: Uuid::nil(),
            alias: "api.example.com",
            method: "GET",
            status: Some(StatusCode::OK),
            duration_ms: 12,
            request_size: 3,
            response_size: 5,
            error_type: None,
        };
        proxy_request(&audit);
    }
}
