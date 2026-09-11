// Updated: 2026-09-01 by Constructor Tech
//! GTS (Global Type System) identifiers for the OAGW gear.
//!
//! Every identifier OAGW either registers in the types-registry or accepts on
//! the wire lives here so the strings have a single source of truth. The
//! `gts_id!` macro builds the `gts.`-prefixed form from the bare suffix.

use toolkit_gts::gts_id;

// ── Resource base types ─────────────────────────────────────────────────────

/// Upstream resource base type (`gts.cf.core.oagw.upstream.v1~`).
pub const UPSTREAM_TYPE: &str = gts_id!("cf.core.oagw.upstream.v1~");
/// Route resource base type (`gts.cf.core.oagw.route.v1~`).
pub const ROUTE_TYPE: &str = gts_id!("cf.core.oagw.route.v1~");
/// Proxy capability resource type (`gts.cf.core.oagw.proxy.v1~`).
pub const PROXY_TYPE: &str = gts_id!("cf.core.oagw.proxy.v1~");
/// Auth plugin base type.
pub const AUTH_PLUGIN_TYPE: &str = gts_id!("cf.core.oagw.auth_plugin.v1~");
/// Guard plugin base type.
pub const GUARD_PLUGIN_TYPE: &str = gts_id!("cf.core.oagw.guard_plugin.v1~");
/// Transform plugin base type.
pub const TRANSFORM_PLUGIN_TYPE: &str = gts_id!("cf.core.oagw.transform_plugin.v1~");

// ── Protocols ───────────────────────────────────────────────────────────────

/// HTTP protocol identifier.
pub const PROTOCOL_HTTP: &str = gts_id!("cf.core.oagw.protocol.v1~cf.core.oagw.http.v1");
/// gRPC protocol identifier (catalogued; no proxy code path is reachable).
pub const PROTOCOL_GRPC: &str = gts_id!("cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1");

// ── Built-in auth plugin identifiers (resolvable) ───────────────────────────

pub const AUTH_NOOP: &str = gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1");
pub const AUTH_APIKEY: &str = gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1");
pub const AUTH_OAUTH2_CC: &str =
    gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1");
pub const AUTH_OAUTH2_CC_BASIC: &str =
    gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1");

// ── Catalog-only auth plugin identifiers (reserved, no implementation) ──────

pub const AUTH_BASIC: &str = gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1");
pub const AUTH_BEARER: &str = gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1");

// ── Built-in guard plugin identifiers ───────────────────────────────────────

pub const GUARD_REQUIRED_HEADERS: &str =
    gts_id!("cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1");

/// Catalog-only: timeout is core Data Plane configuration.
pub const GUARD_TIMEOUT: &str = gts_id!("cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1");
/// Catalog-only: CORS is core Data Plane configuration via `cors`.
pub const GUARD_CORS: &str = gts_id!("cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1");

// ── Built-in transform plugin identifiers ───────────────────────────────────

pub const TRANSFORM_REQUEST_ID: &str =
    gts_id!("cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1");

/// Catalog-only: request/response logging is core Data Plane instrumentation.
pub const TRANSFORM_LOGGING: &str =
    gts_id!("cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1");
/// Catalog-only: metrics collection is core Data Plane instrumentation.
pub const TRANSFORM_METRICS: &str =
    gts_id!("cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1");

// ── Error type identifiers (RFC 9457 `type` URIs) ───────────────────────────
//
// These are spelled out rather than built with `gts_id!` because they chain
// off the *canonical error* base (`cf.core.errors.err.v1~`) rather than an
// OAGW resource base, and the exact wire spelling is pinned by DESIGN §3.3.

pub const ERR_PREFIX: &str = "gts.cf.core.errors.err.v1~cf.oagw";

/// `gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1`
pub const ERR_VALIDATION: &str = "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1`
pub const ERR_MISSING_TARGET_HOST: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1`
pub const ERR_INVALID_TARGET_HOST: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1`
pub const ERR_UNKNOWN_TARGET_HOST: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1`
pub const ERR_AUTH_FAILED: &str = "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1`
pub const ERR_ROUTE_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.upstream.not_found.v1`
pub const ERR_UPSTREAM_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.upstream.not_found.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.resource.not_found.v1`
pub const ERR_RESOURCE_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.resource.not_found.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.resource.conflict.v1`
pub const ERR_RESOURCE_CONFLICT: &str = "gts.cf.core.errors.err.v1~cf.oagw.resource.conflict.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1`
pub const ERR_PLUGIN_IN_USE: &str = "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1`
pub const ERR_PAYLOAD_TOO_LARGE: &str = "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1`
pub const ERR_RATE_LIMIT_EXCEEDED: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1`
pub const ERR_SECRET_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1`
pub const ERR_PROTOCOL_ERROR: &str = "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1`
pub const ERR_DOWNSTREAM_ERROR: &str = "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1`
pub const ERR_STREAM_ABORTED: &str = "gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1`
pub const ERR_LINK_UNAVAILABLE: &str = "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1`
pub const ERR_CIRCUIT_BREAKER_OPEN: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1`
pub const ERR_PLUGIN_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1`
pub const ERR_CONNECTION_TIMEOUT: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1`
pub const ERR_REQUEST_TIMEOUT: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1`
pub const ERR_IDLE_TIMEOUT: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1`
pub const ERR_CORS_ORIGIN_NOT_ALLOWED: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1`
pub const ERR_CORS_METHOD_NOT_ALLOWED: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.forbidden.v1`
pub const ERR_FORBIDDEN: &str = "gts.cf.core.errors.err.v1~cf.oagw.forbidden.v1";
/// `gts.cf.core.errors.err.v1~cf.oagw.internal.error.v1`
pub const ERR_INTERNAL: &str = "gts.cf.core.errors.err.v1~cf.oagw.internal.error.v1";

/// Build the anonymous GTS instance identifier for a resource:
/// `gts.cf.core.oagw.<type>.v1~<uuid>`.
///
/// The UUID is in its `simple` form — 32 lowercase hex digits, no hyphens —
/// matching the instance identifiers the rest of the platform mints.
#[must_use]
pub fn instance_id(base_type: &str, id: uuid::Uuid) -> String {
    format!("{base_type}{}", id.simple())
}

/// Strip the GTS base type from an instance identifier, returning the bare
/// UUID suffix.
#[must_use]
pub fn uuid_of(instance: &str) -> Option<uuid::Uuid> {
    instance.rsplit('~').next().and_then(|s| s.parse().ok())
}

/// Human-readable title for each of the [`ERR_*`](self) identifiers above.
pub fn error_title(type_id: &str) -> &'static str {
    match type_id {
        ERR_VALIDATION => "Validation Error",
        ERR_MISSING_TARGET_HOST => "Missing Target Host Header",
        ERR_INVALID_TARGET_HOST => "Invalid Target Host Format",
        ERR_UNKNOWN_TARGET_HOST => "Unknown Target Host",
        ERR_AUTH_FAILED => "Authentication Failed",
        ERR_ROUTE_NOT_FOUND => "Route Not Found",
        ERR_UPSTREAM_NOT_FOUND => "Upstream Not Found",
        ERR_RESOURCE_NOT_FOUND => "Resource Not Found",
        ERR_RESOURCE_CONFLICT => "Resource Conflict",
        ERR_PLUGIN_IN_USE => "Plugin In Use",
        ERR_PAYLOAD_TOO_LARGE => "Payload Too Large",
        ERR_RATE_LIMIT_EXCEEDED => "Rate Limit Exceeded",
        ERR_SECRET_NOT_FOUND => "Secret Not Found",
        ERR_PROTOCOL_ERROR => "Protocol Error",
        ERR_DOWNSTREAM_ERROR => "Downstream Error",
        ERR_STREAM_ABORTED => "Stream Aborted",
        ERR_LINK_UNAVAILABLE => "Link Unavailable",
        ERR_CIRCUIT_BREAKER_OPEN => "Circuit Breaker Open",
        ERR_PLUGIN_NOT_FOUND => "Plugin Not Found",
        ERR_CONNECTION_TIMEOUT => "Connection Timeout",
        ERR_REQUEST_TIMEOUT => "Request Timeout",
        ERR_IDLE_TIMEOUT => "Idle Timeout",
        ERR_CORS_ORIGIN_NOT_ALLOWED => "CORS Origin Not Allowed",
        ERR_CORS_METHOD_NOT_ALLOWED => "CORS Method Not Allowed",
        ERR_FORBIDDEN => "Forbidden",
        ERR_INTERNAL => "Internal Error",
        _ => "Error",
    }
}
