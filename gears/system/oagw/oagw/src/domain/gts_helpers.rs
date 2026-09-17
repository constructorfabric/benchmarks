//! GTS (Global Type System) identifiers and helpers for the `oagw` gear.
//!
//! Every OAGW resource is named by a *type id* (trailing `~`) and instantiated
//! as an *instance id* of the form `<type-id><instance-suffix>`. Instance ids
//! are what the wire carries (`upstream_id`, `plugin_id`, ...), so this module
//! also owns the parse/format round trip between bare UUIDs and GTS instance
//! ids.
//!
//! # Single source of truth
//!
//! The type ids below mirror the JSON Schemas in `docs/schemas/` and the
//! built-in plugin catalog in PRD.md. `infra/type_provisioning.rs` publishes
//! the schemas to the types-registry under exactly these ids.

use uuid::Uuid;

// ---------------------------------------------------------------------------
// Resource type ids (the *types*, trailing `~`)
// ---------------------------------------------------------------------------

/// Type id of an OAGW upstream (`docs/schemas/upstream.v1.schema.json`).
pub const OAGW_UPSTREAM_TYPE_ID: &str = "gts.cf.core.oagw.upstream.v1~";
/// Type id of an OAGW route (`docs/schemas/route.v1.schema.json`).
pub const OAGW_ROUTE_TYPE_ID: &str = "gts.cf.core.oagw.route.v1~";
/// Type id of an OAGW auth plugin.
pub const OAGW_AUTH_PLUGIN_TYPE_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~";
/// Type id of an OAGW guard plugin.
pub const OAGW_GUARD_PLUGIN_TYPE_ID: &str = "gts.cf.core.oagw.guard_plugin.v1~";
/// Type id of an OAGW transform plugin.
pub const OAGW_TRANSFORM_PLUGIN_TYPE_ID: &str = "gts.cf.core.oagw.transform_plugin.v1~";
/// Type id of an OAGW upstream protocol (the `protocol` discriminator).
pub const OAGW_PROTOCOL_TYPE_ID: &str = "gts.cf.core.oagw.protocol.v1~";

// ---------------------------------------------------------------------------
// Protocol instance ids (`upstream.protocol`)
// ---------------------------------------------------------------------------

/// HTTP/1.1 and HTTP/2 upstream protocol.
pub const PROTOCOL_HTTP_ID: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
/// gRPC upstream protocol.
pub const PROTOCOL_GRPC_ID: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";

// ---------------------------------------------------------------------------
// Built-in plugin ids (PRD "plugin catalog")
// ---------------------------------------------------------------------------

/// Built-in auth plugin: no-op (anonymous / no upstream authentication).
pub const NOOP_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1";
/// Built-in auth plugin: static API key sourced from the credstore.
pub const APIKEY_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
/// Built-in auth plugin: OAuth2 client-credentials flow (`client_secret_post`).
pub const OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";
/// Built-in auth plugin: OAuth2 client-credentials with HTTP Basic
/// (`client_secret_basic`) at the token endpoint.
pub const OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1";
/// Catalog-only auth plugin: HTTP Basic against the upstream. Reserved GTS
/// identifier with **no** backing `AuthPlugin` implementation.
pub const BASIC_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1";
/// Catalog-only auth plugin: static bearer token against the upstream.
/// Reserved GTS identifier with **no** backing `AuthPlugin` implementation.
pub const BEARER_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1";

/// Built-in guard plugin: required headers on the request and/or response.
pub const REQUIRED_HEADERS_GUARD_PLUGIN_ID: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
/// Catalog-only guard identifier: request timeout (core Data Plane config).
pub const TIMEOUT_GUARD_PLUGIN_ID: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1";
/// Catalog-only guard identifier: CORS (core Data Plane config via `cors`).
pub const CORS_GUARD_PLUGIN_ID: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1";

/// Built-in transform plugin: `X-Request-Id` injection/propagation.
pub const REQUEST_ID_TRANSFORM_PLUGIN_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";
/// Catalog-only transform identifier: structured access logging.
pub const LOGGING_TRANSFORM_PLUGIN_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1";
/// Catalog-only transform identifier: request/response metrics.
pub const METRICS_TRANSFORM_PLUGIN_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1";

// ---------------------------------------------------------------------------
// Problem-type ids (DESIGN.md "error response" table)
// ---------------------------------------------------------------------------

/// OAGW problem type ids and their RFC 9457 `title`s.
///
/// The ids are *OAGW-scoped refinements* of the platform's AIP-193 error
/// categories; the HTTP status for each is owned by [`crate::domain::error`].
///
/// The literal ids mirror DESIGN.md's error table exactly (note the
/// underscores: `rate_limit.exceeded.v1`, not `rate.limit.exceeded.v1`).
pub mod problem {
    /// Base prefix shared by every OAGW problem type id.
    pub const PREFIX: &str = "gts.cf.core.errors.err.v1~cf.oagw.";

    /// Build an OAGW problem type id from its `PREFIX`-relative suffix.
    ///
    /// `concat!` only accepts literals, so the prefix is spelled out here and
    /// [`PREFIX`] above is the same string for runtime use.
    macro_rules! problem_type {
        ($suffix:literal) => {
            concat!("gts.cf.core.errors.err.v1~cf.oagw.", $suffix)
        };
    }

    /// Request body / query validation failure (400).
    pub const VALIDATION_ERROR: &str = problem_type!("validation.error.v1");
    /// The upstream has no usable endpoint for the resolved alias (400).
    pub const MISSING_TARGET_HOST: &str = problem_type!("routing.missing_target_host.v1");
    /// The `X-OAGW-Target-Host` value is malformed (400).
    pub const INVALID_TARGET_HOST: &str = problem_type!("routing.invalid_target_host.v1");
    /// The `X-OAGW-Target-Host` value matches no configured endpoint (400).
    pub const UNKNOWN_TARGET_HOST: &str = problem_type!("routing.unknown_target_host.v1");
    /// The alias may not be changed (400).
    pub const ALIAS_IMMUTABLE: &str = problem_type!("alias.immutable.v1");
    /// A route's `upstream_id` may not be changed (400).
    pub const UPSTREAM_ID_IMMUTABLE: &str = problem_type!("route.upstream_immutable.v1");
    /// The request method is not served by any matching route (405).
    pub const ROUTE_METHOD_NOT_ALLOWED: &str = problem_type!("route.method_not_allowed.v1");
    /// Credentials missing or invalid (401).
    pub const AUTHENTICATION_FAILED: &str = problem_type!("auth.failed.v1");
    /// No route matched the request, or the referenced route is gone (404).
    pub const ROUTE_NOT_FOUND: &str = problem_type!("route.not_found.v1");
    /// The referenced upstream does not exist (404).
    pub const UPSTREAM_NOT_FOUND: &str = problem_type!("upstream.not_found.v1");
    /// The referenced plugin does not exist (404 management / 503 proxy).
    pub const PLUGIN_NOT_FOUND: &str = problem_type!("plugin.not_found.v1");
    /// The alias is already used in this tenant (409).
    pub const ALIAS_CONFLICT: &str = problem_type!("alias.conflict.v1");
    /// A route's `match` duplicates an existing route (409).
    pub const ROUTE_MATCH_CONFLICT: &str = problem_type!("route.match_conflict.v1");
    /// The plugin is still bound to upstreams/routes (409).
    pub const PLUGIN_IN_USE: &str = problem_type!("plugin.in_use.v1");
    /// The request body is above the configured limit (413).
    pub const PAYLOAD_TOO_LARGE: &str = problem_type!("payload.too_large.v1");
    /// The rate limit is exhausted (429).
    pub const RATE_LIMIT_EXCEEDED: &str = problem_type!("rate_limit.exceeded.v1");
    /// A credstore secret backing a plugin is unreadable (500).
    pub const SECRET_NOT_FOUND: &str = problem_type!("secret.not_found.v1");
    /// Unmapped server-side failure (500).
    pub const INTERNAL: &str = problem_type!("internal.error.v1");
    /// The upstream answered in a way the protocol layer cannot interpret (502).
    pub const PROTOCOL_ERROR: &str = problem_type!("protocol.error.v1");
    /// The upstream returned an error response (502).
    pub const DOWNSTREAM_ERROR: &str = problem_type!("downstream.error.v1");
    /// A streaming exchange aborted mid-flight (502).
    pub const STREAM_ABORTED: &str = problem_type!("stream.aborted.v1");
    /// The upstream is administratively disabled (503).
    pub const UPSTREAM_DISABLED: &str = problem_type!("upstream.disabled.v1");
    /// The upstream is unreachable (503).
    pub const LINK_UNAVAILABLE: &str = problem_type!("link.unavailable.v1");
    /// The circuit breaker for the upstream is open (503).
    pub const CIRCUIT_BREAKER_OPEN: &str = problem_type!("circuit_breaker.open.v1");
    /// No connection could be established in time (504).
    pub const CONNECTION_TIMEOUT: &str = problem_type!("timeout.connection.v1");
    /// The full upstream response was not received in time (504).
    pub const REQUEST_TIMEOUT: &str = problem_type!("timeout.request.v1");
    /// A streaming exchange went idle past the idle timeout (504).
    pub const IDLE_TIMEOUT: &str = problem_type!("timeout.idle.v1");
    /// The request origin is not in the upstream's `allowed_origins` (403).
    pub const CORS_ORIGIN_NOT_ALLOWED: &str = problem_type!("cors.origin_not_allowed.v1");
    /// The request method is not in the upstream's `allowed_methods` (403).
    pub const CORS_METHOD_NOT_ALLOWED: &str = problem_type!("cors.method_not_allowed.v1");
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a GTS *instance* id for a resource: `<type-id><uuid>`.
///
/// ```text
/// gts.cf.core.oagw.upstream.v1~ + 0f9c... -> gts.cf.core.oagw.upstream.v1~0f9c...
/// ```
#[must_use]
pub fn gts_instance_id(type_id: &str, id: &Uuid) -> String {
    format!("{type_id}{id}")
}

/// Parse a wire resource id, accepting either a bare UUID or a full GTS
/// instance id (everything up to and including the last `~` is the type).
///
/// Returns `None` when the input is neither.
#[must_use]
pub fn parse_gts_instance_id(value: &str) -> Option<Uuid> {
    let trimmed = value.trim();
    if let Some(pos) = trimmed.rfind('~') {
        Uuid::parse_str(&trimmed[pos + 1..]).ok()
    } else {
        Uuid::parse_str(trimmed).ok()
    }
}

/// True when `candidate` is a syntactically valid GTS type id (dotted
/// lower-case path terminated by `~`).
#[must_use]
pub fn is_gts_type_id(candidate: &str) -> bool {
    let Some(stripped) = candidate.strip_suffix('~') else {
        return false;
    };
    if stripped.is_empty() {
        return false;
    }
    stripped.split('.').all(|part| {
        !part.is_empty()
            && part
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_ids_round_trip() {
        let id = Uuid::new_v4();
        let full = gts_instance_id(OAGW_UPSTREAM_TYPE_ID, &id);
        assert!(full.starts_with(OAGW_UPSTREAM_TYPE_ID));
        assert_eq!(parse_gts_instance_id(&full), Some(id));
        assert_eq!(parse_gts_instance_id(&id.to_string()), Some(id));
        assert_eq!(parse_gts_instance_id("not-an-id"), None);
    }

    #[test]
    fn gts_type_validation() {
        assert!(is_gts_type_id(OAGW_UPSTREAM_TYPE_ID));
        assert!(!is_gts_type_id("gts.cf.core.oagw.upstream.v1"));
        assert!(!is_gts_type_id("~"));
    }

    #[test]
    fn protocol_ids_use_the_protocol_type() {
        for id in [PROTOCOL_HTTP_ID, PROTOCOL_GRPC_ID] {
            assert!(id.starts_with(OAGW_PROTOCOL_TYPE_ID));
        }
    }
}
