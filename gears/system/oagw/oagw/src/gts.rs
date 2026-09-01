//! GTS identifier constants and helpers for the OAGW gear.
//!
//! Every resource and every error surfaced by OAGW carries a Global Type
//! System (GTS) identifier. Resource instances use anonymous identifiers of
//! the form `gts.cf.core.oagw.{type}.v1~{uuid}`; errors use the shared error
//! base type `gts.cf.core.errors.err.v1~`.

/// GTS type identifier for upstream resources (the `~` suffix marks a type id
/// rather than an instance id).
pub const UPSTREAM_TYPE_ID: &str = "gts.cf.core.oagw.upstream.v1~";
/// GTS type identifier for route resources.
pub const ROUTE_TYPE_ID: &str = "gts.cf.core.oagw.route.v1~";
/// GTS type identifier for auth plugin resources.
pub const AUTH_PLUGIN_TYPE_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~";
/// GTS type identifier for guard plugin resources.
pub const GUARD_PLUGIN_TYPE_ID: &str = "gts.cf.core.oagw.guard_plugin.v1~";
/// GTS type identifier for transform plugin resources.
pub const TRANSFORM_PLUGIN_TYPE_ID: &str = "gts.cf.core.oagw.transform_plugin.v1~";
/// GTS type identifier for the protocol catalog.
pub const PROTOCOL_TYPE_ID: &str = "gts.cf.core.oagw.protocol.v1~";

/// GTS instance identifier of the HTTP protocol.
pub const PROTOCOL_HTTP_ID: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
/// GTS instance identifier of the gRPC protocol.
pub const PROTOCOL_GRPC_ID: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";

// ---------------------------------------------------------------------------
// Built-in auth plugin identifiers
// ---------------------------------------------------------------------------
/// No-op auth plugin (no credential injection).
pub const AUTH_NOOP_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1";
/// API-key auth plugin.
pub const AUTH_APIKEY_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
/// `OAuth2` client-credentials auth plugin (Form client auth).
pub const AUTH_OAUTH2_ID: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";
/// `OAuth2` client-credentials auth plugin (Basic client auth).
pub const AUTH_OAUTH2_BASIC_ID: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1";
/// Catalog-only identifiers (reserved, no backing implementation).
pub const AUTH_BASIC_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1";
pub const AUTH_BEARER_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1";

// ---------------------------------------------------------------------------
// Built-in guard plugin identifiers
// ---------------------------------------------------------------------------
/// Required headers guard plugin.
pub const GUARD_REQUIRED_HEADERS_ID: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
/// Catalog-only identifiers (core data-plane logic, not bindable).
pub const GUARD_TIMEOUT_ID: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1";
pub const GUARD_CORS_ID: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1";

// ---------------------------------------------------------------------------
// Built-in transform plugin identifiers
// ---------------------------------------------------------------------------
/// X-Request-ID propagation transform plugin.
pub const TRANSFORM_REQUEST_ID_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";
/// Catalog-only identifiers (core data-plane instrumentation, not bindable).
pub const TRANSFORM_LOGGING_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1";
pub const TRANSFORM_METRICS_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1";

// ---------------------------------------------------------------------------
// Error GTS instance identifiers (shared `gts.cf.core.errors.err.v1~` base)
// ---------------------------------------------------------------------------

/// 400 — general route / request validation error.
pub const ERR_VALIDATION: &str = "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1";
/// 403 — the caller lacks a management permission (generic access denied).
pub const ERR_FORBIDDEN: &str = "gts.cf.core.errors.err.v1~cf.oagw.access_denied.v1";
/// 409 — a lifecycle/uniqueness constraint was violated (generic conflict).
pub const ERR_CONFLICT: &str = "gts.cf.core.errors.err.v1~cf.oagw.conflict.v1";
/// 400 — `X-OAGW-Target-Host` required but absent (common-suffix pools).
pub const ERR_MISSING_TARGET_HOST: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1";
/// 400 — `X-OAGW-Target-Host` malformed.
pub const ERR_INVALID_TARGET_HOST: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1";
/// 400 — `X-OAGW-Target-Host` does not match any configured endpoint.
pub const ERR_UNKNOWN_TARGET_HOST: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1";
/// 401 — authentication against the upstream failed.
pub const ERR_AUTH_FAILED: &str = "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1";
/// 403 — CORS origin not allowed.
pub const ERR_CORS_ORIGIN: &str = "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1";
/// 403 — CORS method not allowed.
pub const ERR_CORS_METHOD: &str = "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1";
/// 404 — no matching route / upstream found.
pub const ERR_ROUTE_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1";
/// 409 — plugin still referenced by an upstream or route.
pub const ERR_PLUGIN_IN_USE: &str = "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1";
/// 413 — request payload exceeds the 100 MB hard limit.
pub const ERR_PAYLOAD_TOO_LARGE: &str = "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1";
/// 429 — rate limit exceeded.
pub const ERR_RATE_LIMIT: &str = "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1";
/// 500 — a referenced secret could not be resolved.
pub const ERR_SECRET_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1";
/// 502 — protocol-level error talking to the upstream.
pub const ERR_PROTOCOL: &str = "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1";
/// 502 — generic upstream (downstream) error.
pub const ERR_DOWNSTREAM: &str = "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1";
/// 502 — a stream connection was aborted mid-flight.
pub const ERR_STREAM_ABORTED: &str = "gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1";
/// 503 — upstream link unavailable (e.g. disabled upstream).
pub const ERR_LINK_UNAVAILABLE: &str = "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1";
/// 503 — circuit breaker open.
pub const ERR_CIRCUIT_BREAKER: &str = "gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1";
/// 503 — a referenced plugin cannot be resolved.
pub const ERR_PLUGIN_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1";
/// 504 — connection timeout.
pub const ERR_CONNECTION_TIMEOUT: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1";
/// 504 — overall request timeout.
pub const ERR_REQUEST_TIMEOUT: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1";
/// 504 — idle timeout while streaming.
pub const ERR_IDLE_TIMEOUT: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1";
/// 500 — internal gateway error (never leaks secrets).
pub const ERR_INTERNAL: &str = "gts.cf.core.errors.err.v1~cf.oagw.internal.v1";

/// Build the anonymous GTS instance identifier for a resource.
///
/// `type_id` is one of the `*_TYPE_ID` constants above; the returned id is
/// `gts.cf.core.oagw.{type}.v1~{uuid}`.
#[must_use]
pub fn resource_instance_id(type_id: &str, uuid: &uuid::Uuid) -> String {
    format!("{type_id}{uuid}")
}

/// Extract the bare UUID out of a resource identifier.
///
/// Accepts both a full GTS instance id (`...~{uuid}`) and a bare UUID string.
/// Returns `None` when neither parse succeeds.
#[must_use]
pub fn parse_resource_id(raw: &str) -> Option<uuid::Uuid> {
    match raw.rsplit_once('~') {
        Some((_, tail)) => uuid::Uuid::parse_str(tail).ok(),
        None => uuid::Uuid::parse_str(raw).ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_id_builds_full_gts_string() {
        let id = uuid::Uuid::new_v4();
        let full = resource_instance_id(UPSTREAM_TYPE_ID, &id);
        assert_eq!(full, format!("gts.cf.core.oagw.upstream.v1~{id}"));
        assert!(full.starts_with("gts.cf.core.oagw.upstream.v1~"));
    }

    #[test]
    fn parse_accepts_gts_and_bare_uuid() {
        let id = uuid::Uuid::new_v4();
        let full = resource_instance_id(ROUTE_TYPE_ID, &id);
        assert_eq!(parse_resource_id(&full), Some(id));
        assert_eq!(parse_resource_id(&id.to_string()), Some(id));
        assert_eq!(parse_resource_id("not-an-id"), None);
        assert_eq!(parse_resource_id("gts.cf.core.oagw.upstream.v1~junk"), None);
    }
}
