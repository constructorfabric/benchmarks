//! GTS identifiers used by the `oagw` gear.
//!
//! Every persisted resource is identified by a GTS identifier of the form
//! `gts.<type>~<instance>`; management API responses emit the full form and
//! accept either the full form or the bare instance UUID on input.

/// Type prefix for upstream configuration objects.
pub const UPSTREAM_TYPE: &str = "gts.cf.core.oagw.upstream.v1~";
/// Type prefix for route configuration objects.
pub const ROUTE_TYPE: &str = "gts.cf.core.oagw.route.v1~";
/// Type prefix for custom (Starlark) plugin definitions.
pub const PLUGIN_TYPE: &str = "gts.cf.core.oagw.plugin.v1~";

/// HTTP protocol identifier.
pub const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
/// gRPC protocol identifier.
pub const PROTOCOL_GRPC: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";

/// Built-in auth plugin identifiers.
pub mod auth_plugin {
    /// No authentication.
    pub const NOOP: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1";
    /// Static API key injection.
    pub const APIKEY: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
    /// Static Basic auth (cataloged; no built-in implementation).
    pub const BASIC: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1";
    /// Static bearer token (cataloged; no built-in implementation).
    pub const BEARER: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1";
    /// OAuth2 client credentials, credentials in the request body.
    pub const OAUTH2_CLIENT_CRED: &str =
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";
    /// OAuth2 client credentials, credentials in the `Authorization` header.
    pub const OAUTH2_CLIENT_CRED_BASIC: &str =
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1";
}

/// Built-in guard plugin identifiers.
pub mod guard_plugin {
    /// Required header enforcement.
    pub const REQUIRED_HEADERS: &str =
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
    /// Request timeout (core data plane logic, cataloged only).
    pub const TIMEOUT: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1";
    /// CORS (core data plane logic, cataloged only).
    pub const CORS: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1";
}

/// Built-in transform plugin identifiers.
pub mod transform_plugin {
    /// `X-Request-ID` propagation.
    pub const REQUEST_ID: &str = "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";
    /// Logging (core data plane instrumentation, cataloged only).
    pub const LOGGING: &str = "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1";
    /// Prometheus metrics (core data plane instrumentation, cataloged only).
    pub const METRICS: &str = "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1";
}

/// Error type identifiers emitted by the data plane (`cf.oagw.*`).
pub mod errors {
    /// Base of every OAGW error type identifier.
    pub const BASE: &str = "gts.cf.core.errors.err.v1~cf.oagw.";

    /// 400 — request validation failed.
    pub const VALIDATION_ERROR: &str = "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1";
    /// 400 — `X-OAGW-Target-Host` required but absent.
    pub const ROUTING_MISSING_TARGET_HOST: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1";
    /// 400 — `X-OAGW-Target-Host` malformed.
    pub const ROUTING_INVALID_TARGET_HOST: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1";
    /// 400 — `X-OAGW-Target-Host` names no configured endpoint.
    pub const ROUTING_UNKNOWN_TARGET_HOST: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1";
    /// 401 — authentication to the upstream failed.
    pub const AUTH_FAILED: &str = "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1";
    /// 404 — no matching route.
    pub const ROUTE_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1";
    /// 409 — plugin still bound to an upstream or route.
    pub const PLUGIN_IN_USE: &str = "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1";
    /// 413 — request payload exceeds the configured limit.
    pub const PAYLOAD_TOO_LARGE: &str = "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1";
    /// 429 — rate limit exhausted.
    pub const RATE_LIMIT_EXCEEDED: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1";
    /// 500 — referenced secret missing.
    pub const SECRET_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1";
    /// 502 — protocol-level failure.
    pub const PROTOCOL_ERROR: &str = "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1";
    /// 502 — the upstream service failed.
    pub const DOWNSTREAM_ERROR: &str = "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1";
    /// 502 — a streamed connection was aborted mid-flight.
    pub const STREAM_ABORTED: &str = "gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1";
    /// 503 — the upstream link is unavailable.
    pub const LINK_UNAVAILABLE: &str = "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1";
    /// 503 — the circuit breaker is open.
    pub const CIRCUIT_BREAKER_OPEN: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1";
    /// 503 — a bound plugin could not be resolved.
    pub const PLUGIN_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1";
    /// 504 — connection could not be established in time.
    pub const TIMEOUT_CONNECTION: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1";
    /// 504 — the upstream did not produce a response in time.
    pub const TIMEOUT_REQUEST: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1";
    /// 504 — the upstream stopped sending data mid-response.
    pub const TIMEOUT_IDLE: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1";
    /// 403 — CORS origin rejected.
    pub const CORS_ORIGIN_NOT_ALLOWED: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1";
    /// 403 — CORS method rejected.
    pub const CORS_METHOD_NOT_ALLOWED: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1";
}

/// Formats a bare instance UUID into the full GTS identifier for a type.
#[must_use]
pub fn qualify(prefix: &str, id: &str) -> String {
    let id = id.strip_prefix("gts://").unwrap_or(id);
    if id.starts_with(prefix) {
        id.to_owned()
    } else {
        format!("{prefix}{id}")
    }
}

/// Strips the type prefix from a GTS identifier, returning the instance part.
#[must_use]
pub fn unqualify<'a>(prefix: &str, id: &'a str) -> &'a str {
    let id = id.strip_prefix("gts://").unwrap_or(id);
    id.strip_prefix(prefix).unwrap_or(id)
}
