//! GTS identifier constants for the OAGW gear.
//!
//! Single source of truth for every GTS string the gear emits: resource
//! types (PEP target types / management-API `resource_type` fields),
//! plugin type identifiers and well-known builtin plugin instance ids,
//! protocol ids, and the `cf.oagw.*` error type catalogue (`DOCS §8.1`).
//!
//! Identifier layout (all through `toolkit_gts::gts_id!`):
//!
//! ```text
//! gts.cf.core.oagw.<resource>.v1~                       resource type (base)
//! gts.cf.core.oagw.<resource>.v1~cf.core.oagw.<name>.v1  builtin plugin instance
//! gts.cf.core.errors.err.v1~cf.oagw.<kind>.<name>.v1     error type
//! ```

use toolkit_gts::gts_id;

// ---------------------------------------------------------------------------
// Resource / PEP target types
// ---------------------------------------------------------------------------

/// Upstream resource type — PEP target + management-API resource id prefix.
pub const UPSTREAM_TYPE_ID: &str = gts_id!("cf.core.oagw.upstream.v1~");

/// Route resource type.
pub const ROUTE_TYPE_ID: &str = gts_id!("cf.core.oagw.route.v1~");

/// Auth plugin resource type.
pub const AUTH_PLUGIN_TYPE_ID: &str = gts_id!("cf.core.oagw.auth_plugin.v1~");

/// Guard plugin resource type.
pub const GUARD_PLUGIN_TYPE_ID: &str = gts_id!("cf.core.oagw.guard_plugin.v1~");

/// Transform plugin resource type.
pub const TRANSFORM_PLUGIN_TYPE_ID: &str = gts_id!("cf.core.oagw.transform_plugin.v1~");

/// Proxy (data plane) resource type — `:invoke` permission.
pub const PROXY_TYPE_ID: &str = gts_id!("cf.core.oagw.proxy.v1~");

/// Custom plugin resource type (the stored plugin definition).
pub const PLUGIN_TYPE_ID: &str = gts_id!("cf.core.oagw.plugin.v1~");

/// Connection protocol namespace (type-schema for the protocol instances).
pub const PROTOCOL_TYPE_ID: &str = gts_id!("cf.core.oagw.protocol.v1~");

/// Upstream connection protocols.
pub const PROTOCOL_HTTP_ID: &str = gts_id!("cf.core.oagw.protocol.v1~cf.core.oagw.http.v1");
pub const PROTOCOL_GRPC_ID: &str = gts_id!("cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1");

/// Generic plugin spec envelope (derived from `PluginV1`) — the type every
/// builtin OAGW plugin instance conforms to.
pub const OAGW_PLUGIN_SPEC_TYPE_ID: &str =
    gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin.v1~");

// ---------------------------------------------------------------------------
// Well-known builtin plugin instances (resolvable in-process)
// ---------------------------------------------------------------------------

/// Auth — no authentication (default / no-op).
pub const AUTH_PLUGIN_NOOP: &str = gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1");

/// Auth — API key injection via header or query (`cred://` reference).
pub const AUTH_PLUGIN_APIKEY: &str = gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1");

/// Auth — `OAuth2` client-credentials, credentials in the request form body.
pub const AUTH_PLUGIN_OAUTH2_CLIENT_CRED: &str =
    gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1");

/// Auth — `OAuth2` client-credentials, credentials via `Authorization: Basic`.
pub const AUTH_PLUGIN_OAUTH2_CLIENT_CRED_BASIC: &str =
    gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1");

/// Guard — required headers (request + response presence checks).
pub const GUARD_PLUGIN_REQUIRED_HEADERS: &str =
    gts_id!("cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1");

/// Transform — `X-Request-ID` injection / propagation.
pub const TRANSFORM_PLUGIN_REQUEST_ID: &str =
    gts_id!("cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1");

// ---------------------------------------------------------------------------
// Catalog-only plugin identifiers
//
// Discoverable through the types-registry catalogue but NOT resolvable via
// an in-process plugin registry: binding one of these as a `plugin_ref`
// (or as `auth.type`) fails with "unknown plugin". They are implemented as
// core data-plane behaviour instead (see `DOCS §3.3`).
// ---------------------------------------------------------------------------

/// Auth — HTTP Basic (catalog-only).
pub const AUTH_PLUGIN_BASIC_CATALOG: &str =
    gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1");

/// Auth — Bearer passthrough (catalog-only).
pub const AUTH_PLUGIN_BEARER_CATALOG: &str =
    gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1");

/// Guard — timeout (catalog-only; core Data Plane timeout logic).
pub const GUARD_PLUGIN_TIMEOUT_CATALOG: &str =
    gts_id!("cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1");

/// Guard — CORS (catalog-only; core CORS handler).
pub const GUARD_PLUGIN_CORS_CATALOG: &str =
    gts_id!("cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1");

/// Transform — logging (catalog-only; core instrumentation).
pub const TRANSFORM_PLUGIN_LOGGING_CATALOG: &str =
    gts_id!("cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1");

/// Transform — metrics (catalog-only; core instrumentation).
pub const TRANSFORM_PLUGIN_METRICS_CATALOG: &str =
    gts_id!("cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1");

// ---------------------------------------------------------------------------
// Error type catalogue (RFC 9457 `type` URIs; DOCS §8.1)
// ---------------------------------------------------------------------------

/// Validation failed (400).
pub const ERR_VALIDATION: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.validation.error.v1");

/// `X-OAGW-Target-Host` required but absent (400).
pub const ERR_MISSING_TARGET_HOST: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1");

/// `X-OAGW-Target-Host` present but malformed (400).
pub const ERR_INVALID_TARGET_HOST: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1");

/// `X-OAGW-Target-Host` not among the configured endpoints (400).
pub const ERR_UNKNOWN_TARGET_HOST: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1");

/// Upstream authentication failed (401).
pub const ERR_AUTH_FAILED: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.auth.failed.v1");

/// No route matched (404).
pub const ERR_ROUTE_NOT_FOUND: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.route.not_found.v1");

/// Plugin still referenced by upstreams/routes (409, delete blocked).
pub const ERR_PLUGIN_IN_USE: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1");

/// Request body exceeds the 100MB hard limit (413).
pub const ERR_PAYLOAD_TOO_LARGE: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.payload.too_large.v1");

/// Rate limit exceeded (429).
pub const ERR_RATE_LIMIT_EXCEEDED: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1");

/// Referenced secret is missing (500).
pub const ERR_SECRET_NOT_FOUND: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.secret.not_found.v1");

/// Protocol-level error talking to the upstream (502).
pub const ERR_PROTOCOL: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.protocol.error.v1");

/// Upstream service error (502).
pub const ERR_DOWNSTREAM: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.downstream.error.v1");

/// Stream aborted (502).
pub const ERR_STREAM_ABORTED: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.stream.aborted.v1");

/// Upstream link unavailable (503).
pub const ERR_LINK_UNAVAILABLE: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.link.unavailable.v1");

/// Circuit breaker open (503).
pub const ERR_CIRCUIT_BREAKER_OPEN: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1");

/// Plugin not found (503).
pub const ERR_PLUGIN_NOT_FOUND: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1");

/// Connection timeout (504).
pub const ERR_TIMEOUT_CONNECTION: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.timeout.connection.v1");

/// Request timeout (504).
pub const ERR_TIMEOUT_REQUEST: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.timeout.request.v1");

/// Idle timeout (504).
pub const ERR_TIMEOUT_IDLE: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.timeout.idle.v1");

/// CORS origin not allowed (403).
pub const ERR_CORS_ORIGIN_NOT_ALLOWED: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1");

/// CORS method not allowed (403).
pub const ERR_CORS_METHOD_NOT_ALLOWED: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1");

/// PEP denial — proxy invocation not authorized (403). Canonical
/// `cf.core.err` category, shared with the management plane's
/// `CrossTenantDenied` mapping (no `cf.oagw.*` equivalent exists).
pub const ERR_PERMISSION_DENIED: &str =
    gts_id!("cf.core.errors.err.v1~cf.core.err.permission_denied.v1~");

/// Prefix prepended to the GTS error fragments to form RFC 9457 `type`
/// URIs (matches the canonical `Problem` envelope: `gts://` + fragment).
pub const ERR_URI_PREFIX: &str = "gts://";

/// Build an RFC 9457 `type` URI for one of the OAGW error constants.
#[must_use]
pub fn error_uri(fragment: &str) -> String {
    format!("{ERR_URI_PREFIX}{fragment}")
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn plugin_identifiers_match_documented_shapes() {
        // Builtin plugin ids live under the gear plugin type namespaces.
        assert!(AUTH_PLUGIN_NOOP.starts_with("gts.cf.core.oagw.auth_plugin.v1~"));
        assert!(GUARD_PLUGIN_REQUIRED_HEADERS.starts_with("gts.cf.core.oagw.guard_plugin.v1~"));
        assert!(TRANSFORM_PLUGIN_REQUEST_ID.starts_with("gts.cf.core.oagw.transform_plugin.v1~"));
        // Catalog-only ids share the namespace but are distinct instances.
        assert_ne!(AUTH_PLUGIN_NOOP, AUTH_PLUGIN_BASIC_CATALOG);
        assert_ne!(GUARD_PLUGIN_REQUIRED_HEADERS, GUARD_PLUGIN_CORS_CATALOG);
    }

    #[test]
    fn error_uri_has_gts_scheme() {
        assert_eq!(
            error_uri(ERR_VALIDATION),
            "gts://gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
        );
    }
}
