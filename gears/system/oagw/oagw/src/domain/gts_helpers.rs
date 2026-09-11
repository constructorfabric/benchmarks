//! GTS identifiers OAGW owns, plus the parsing helpers that turn an API path
//! parameter into a UUID.
//!
//! Every plugin, resource and error surfaced by this gear is named by a GTS
//! identifier. Type schemas end in `~`; anything after a `~` is an instance
//! part, which for OAGW is either a named built-in (`cf.core.oagw.<name>.v1`)
//! or a bare UUID for tenant-defined resources.

use uuid::Uuid;

// ---------------------------------------------------------------------------
// Resource type schemas
// ---------------------------------------------------------------------------

/// Base type of an upstream resource.
pub const UPSTREAM_TYPE: &str = "gts.cf.core.oagw.upstream.v1~";
/// Base type of a route resource.
pub const ROUTE_TYPE: &str = "gts.cf.core.oagw.route.v1~";
/// Base type of the proxy capability (permission target only).
pub const PROXY_TYPE: &str = "gts.cf.core.oagw.proxy.v1~";

/// Base type of an auth plugin.
pub const AUTH_PLUGIN_TYPE: &str = "gts.cf.core.oagw.auth_plugin.v1~";
/// Base type of a guard plugin.
pub const GUARD_PLUGIN_TYPE: &str = "gts.cf.core.oagw.guard_plugin.v1~";
/// Base type of a transform plugin.
pub const TRANSFORM_PLUGIN_TYPE: &str = "gts.cf.core.oagw.transform_plugin.v1~";

/// Every plugin base type, in `PluginKind` order.
pub const PLUGIN_TYPES: [&str; 3] = [AUTH_PLUGIN_TYPE, GUARD_PLUGIN_TYPE, TRANSFORM_PLUGIN_TYPE];

// ---------------------------------------------------------------------------
// Protocols
// ---------------------------------------------------------------------------

/// HTTP upstream protocol.
pub const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
/// gRPC upstream protocol (catalogued; no proxy code path — Phase 3).
pub const PROTOCOL_GRPC: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";

// ---------------------------------------------------------------------------
// Built-in plugin identifiers
// ---------------------------------------------------------------------------

/// No-op auth plugin.
pub const NOOP_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1";
/// API-key auth plugin (header or query injection).
pub const APIKEY_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
/// OAuth2 client-credentials auth plugin, `Form` client auth.
pub const OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";
/// OAuth2 client-credentials auth plugin, `Basic` client auth.
pub const OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1";
/// Reserved catalog identifier; no backing implementation.
pub const BASIC_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1";
/// Reserved catalog identifier; no backing implementation.
pub const BEARER_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1";

/// Required-headers guard plugin — the only registry-resolvable guard.
pub const REQUIRED_HEADERS_GUARD_PLUGIN_ID: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
/// Catalog-only: timeout is core Data Plane configuration.
pub const TIMEOUT_GUARD_PLUGIN_ID: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1";
/// Catalog-only: CORS is core Data Plane logic driven by `Upstream.cors`.
pub const CORS_GUARD_PLUGIN_ID: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1";

/// `X-Request-ID` propagation transform plugin.
pub const REQUEST_ID_TRANSFORM_PLUGIN_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";
/// Catalog-only: logging is core Data Plane instrumentation.
pub const LOGGING_TRANSFORM_PLUGIN_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1";
/// Catalog-only: metrics are core Data Plane instrumentation.
pub const METRICS_TRANSFORM_PLUGIN_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1";

/// Identifiers that exist in the types-registry catalog but resolve to no
/// plugin implementation — binding one is a configuration error.
pub const CATALOG_ONLY_PLUGIN_IDS: [&str; 6] = [
    BASIC_AUTH_PLUGIN_ID,
    BEARER_AUTH_PLUGIN_ID,
    TIMEOUT_GUARD_PLUGIN_ID,
    CORS_GUARD_PLUGIN_ID,
    LOGGING_TRANSFORM_PLUGIN_ID,
    METRICS_TRANSFORM_PLUGIN_ID,
];

// ---------------------------------------------------------------------------
// Error type identifiers (`docs/DESIGN.md` §3.3)
// ---------------------------------------------------------------------------

/// GTS identifiers for the RFC 9457 `type` member of every gateway error.
pub mod errors {
    /// Request or configuration validation failure.
    pub const VALIDATION: &str = "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1";
    /// `X-OAGW-Target-Host` is required but absent.
    pub const MISSING_TARGET_HOST: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1";
    /// `X-OAGW-Target-Host` is syntactically invalid.
    pub const INVALID_TARGET_HOST: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1";
    /// `X-OAGW-Target-Host` names no configured endpoint.
    pub const UNKNOWN_TARGET_HOST: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1";
    /// Authentication to the upstream failed.
    pub const AUTH_FAILED: &str = "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1";
    /// No matching upstream or route.
    pub const ROUTE_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1";
    /// CORS origin rejected on an actual (non-preflight) request.
    pub const CORS_ORIGIN_NOT_ALLOWED: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1";
    /// CORS method rejected on an actual (non-preflight) request.
    pub const CORS_METHOD_NOT_ALLOWED: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1";
    /// A resource with the same natural key already exists.
    pub const CONFLICT: &str = "gts.cf.core.errors.err.v1~cf.oagw.conflict.v1";
    /// A plugin cannot be deleted because it is still bound.
    pub const PLUGIN_IN_USE: &str = "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1";
    /// The caller is not permitted to perform the operation.
    pub const FORBIDDEN: &str = "gts.cf.core.errors.err.v1~cf.oagw.forbidden.v1";
    /// Request payload exceeds the hard limit.
    pub const PAYLOAD_TOO_LARGE: &str = "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1";
    /// Rate limit exceeded.
    pub const RATE_LIMIT_EXCEEDED: &str = "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1";
    /// A referenced secret could not be resolved.
    pub const SECRET_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1";
    /// Protocol-level failure talking to the upstream.
    pub const PROTOCOL_ERROR: &str = "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1";
    /// The upstream service itself failed.
    pub const DOWNSTREAM_ERROR: &str = "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1";
    /// A stream was aborted mid-flight.
    pub const STREAM_ABORTED: &str = "gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1";
    /// The upstream link is unavailable (disabled, unreachable).
    pub const LINK_UNAVAILABLE: &str = "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1";
    /// Circuit breaker is open.
    pub const CIRCUIT_BREAKER_OPEN: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1";
    /// A bound plugin could not be resolved.
    pub const PLUGIN_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1";
    /// Connection to the upstream timed out.
    pub const CONNECTION_TIMEOUT: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1";
    /// The upstream exchange exceeded its budget.
    pub const REQUEST_TIMEOUT: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1";
    /// An idle stream exceeded its budget.
    pub const IDLE_TIMEOUT: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1";
    /// The feature is catalogued but not implemented in this build.
    pub const NOT_IMPLEMENTED: &str = "gts.cf.core.errors.err.v1~cf.oagw.not_implemented.v1";
    /// Fallback for an unexpected internal failure.
    pub const INTERNAL: &str = "gts.cf.core.errors.err.v1~cf.oagw.internal.v1";
}

// ---------------------------------------------------------------------------
// Identifier helpers
// ---------------------------------------------------------------------------

/// Format the anonymous GTS identifier for `id` under `base_type`, e.g.
/// `gts.cf.core.oagw.upstream.v1~7c9e6679-…`.
#[must_use]
pub fn anonymous_id(base_type: &str, id: Uuid) -> String {
    format!("{base_type}{id}")
}

/// Instance part of a GTS identifier — everything after the final `~`.
///
/// Returns `None` for a type schema (an id ending in `~`) or an id with no
/// `~` at all.
#[must_use]
pub fn instance_part(gts_id: &str) -> Option<&str> {
    let (_, instance) = gts_id.rsplit_once('~')?;
    (!instance.is_empty()).then_some(instance)
}

/// Base part of a GTS identifier, including the trailing `~`.
#[must_use]
pub fn base_part(gts_id: &str) -> Option<&str> {
    let idx = gts_id.rfind('~')?;
    Some(&gts_id[..=idx])
}

/// Resolve a path parameter to a resource UUID.
///
/// Path parameters are documented as anonymous GTS identifiers
/// (`gts.cf.core.oagw.upstream.v1~{uuid}`) while the entity schemas type the
/// `id` member as a bare `format: uuid`. Both spellings are therefore
/// accepted; a GTS-shaped value must name `base_type`.
#[must_use]
pub fn parse_resource_id(raw: &str, base_type: &str) -> Option<Uuid> {
    let raw = raw.trim();
    if let Ok(id) = Uuid::parse_str(raw) {
        return Some(id);
    }
    let instance = instance_part(raw)?;
    if base_part(raw)? != base_type {
        return None;
    }
    Uuid::parse_str(instance).ok()
}

/// Resolve a plugin path parameter to a UUID under any plugin base type.
#[must_use]
pub fn parse_plugin_id(raw: &str) -> Option<Uuid> {
    PLUGIN_TYPES
        .iter()
        .find_map(|base| parse_resource_id(raw, base))
}

/// Classify a plugin reference: `Some(uuid)` for a UUID-backed custom plugin,
/// `None` for a named built-in resolved through the in-process registry.
///
/// Accepts both the fully-qualified `…_plugin.v1~{uuid}` spelling and a bare
/// UUID (the `plugins.items[]` schema allows either).
#[must_use]
pub fn plugin_ref_uuid(plugin_ref: &str) -> Option<Uuid> {
    if let Ok(id) = Uuid::parse_str(plugin_ref.trim()) {
        return Some(id);
    }
    let instance = instance_part(plugin_ref)?;
    Uuid::parse_str(instance).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_and_base_split() {
        assert_eq!(
            instance_part(NOOP_AUTH_PLUGIN_ID),
            Some("cf.core.oagw.noop.v1")
        );
        assert_eq!(base_part(NOOP_AUTH_PLUGIN_ID), Some(AUTH_PLUGIN_TYPE));
        assert_eq!(instance_part(UPSTREAM_TYPE), None);
        assert_eq!(instance_part("no-tilde"), None);
    }

    #[test]
    fn resource_id_accepts_both_spellings() {
        let id = Uuid::new_v4();
        assert_eq!(parse_resource_id(&id.to_string(), UPSTREAM_TYPE), Some(id));
        assert_eq!(
            parse_resource_id(&anonymous_id(UPSTREAM_TYPE, id), UPSTREAM_TYPE),
            Some(id)
        );
    }

    #[test]
    fn resource_id_rejects_foreign_base_type() {
        let id = Uuid::new_v4();
        assert_eq!(
            parse_resource_id(&anonymous_id(ROUTE_TYPE, id), UPSTREAM_TYPE),
            None
        );
        assert_eq!(parse_resource_id("not-an-id", UPSTREAM_TYPE), None);
    }

    #[test]
    fn plugin_ref_distinguishes_named_from_uuid_backed() {
        let id = Uuid::new_v4();
        assert_eq!(plugin_ref_uuid(NOOP_AUTH_PLUGIN_ID), None);
        assert_eq!(plugin_ref_uuid(&id.to_string()), Some(id));
        assert_eq!(
            plugin_ref_uuid(&anonymous_id(GUARD_PLUGIN_TYPE, id)),
            Some(id)
        );
    }

    #[test]
    fn plugin_id_resolves_under_any_plugin_base() {
        let id = Uuid::new_v4();
        for base in PLUGIN_TYPES {
            assert_eq!(parse_plugin_id(&anonymous_id(base, id)), Some(id));
        }
        assert_eq!(parse_plugin_id(&anonymous_id(UPSTREAM_TYPE, id)), None);
    }
}
