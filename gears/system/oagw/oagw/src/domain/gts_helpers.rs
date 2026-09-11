//! GTS identifiers owned by OAGW and helpers for the anonymous-instance form
//! (`<type>~<uuid>`) used for every resource id on the management API.

use uuid::Uuid;

// ---------------------------------------------------------------------------
// Resource base types
// ---------------------------------------------------------------------------

/// Base type of an upstream configuration object.
pub const UPSTREAM_TYPE: &str = "gts.cf.core.oagw.upstream.v1~";
/// Base type of a route configuration object.
pub const ROUTE_TYPE: &str = "gts.cf.core.oagw.route.v1~";
/// Base type of an auth plugin.
pub const AUTH_PLUGIN_TYPE: &str = "gts.cf.core.oagw.auth_plugin.v1~";
/// Base type of a guard plugin.
pub const GUARD_PLUGIN_TYPE: &str = "gts.cf.core.oagw.guard_plugin.v1~";
/// Base type of a transform plugin.
pub const TRANSFORM_PLUGIN_TYPE: &str = "gts.cf.core.oagw.transform_plugin.v1~";
/// Pseudo-resource guarding the proxy data plane.
pub const PROXY_TYPE: &str = "gts.cf.core.oagw.proxy.v1~";

// ---------------------------------------------------------------------------
// Protocols
// ---------------------------------------------------------------------------

/// HTTP upstream protocol.
pub const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
/// gRPC upstream protocol (catalogued; no proxy code path yet — Phase 3).
pub const PROTOCOL_GRPC: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";

// ---------------------------------------------------------------------------
// Built-in plugin identifiers
// ---------------------------------------------------------------------------

/// `noop` auth plugin — injects nothing.
pub const NOOP_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1";
/// `apikey` auth plugin — header or query API key injection.
pub const APIKEY_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
/// OAuth2 client credentials, credentials in the form body.
pub const OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";
/// OAuth2 client credentials, credentials in the `Authorization` header.
pub const OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1";
/// Reserved catalog identifier — no backing `AuthPlugin` implementation.
pub const BASIC_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1";
/// Reserved catalog identifier — no backing `AuthPlugin` implementation.
pub const BEARER_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1";

/// `required_headers` guard plugin — the only `plugins`-bindable guard.
pub const REQUIRED_HEADERS_GUARD_PLUGIN_ID: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
/// Catalog-only guard identifier; timeout is core Data Plane config.
pub const TIMEOUT_GUARD_PLUGIN_ID: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1";
/// Catalog-only guard identifier; CORS is core Data Plane config.
pub const CORS_GUARD_PLUGIN_ID: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1";

/// `request_id` transform plugin — `X-Request-ID` propagation.
pub const REQUEST_ID_TRANSFORM_PLUGIN_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";
/// Catalog-only transform identifier; logging is core instrumentation.
pub const LOGGING_TRANSFORM_PLUGIN_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1";
/// Catalog-only transform identifier; metrics is core instrumentation.
pub const METRICS_TRANSFORM_PLUGIN_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1";

// ---------------------------------------------------------------------------
// Error type identifiers (RFC 9457 `type` member)
// ---------------------------------------------------------------------------

/// Prefix shared by every canonical OAGW error type identifier.
pub const ERR_PREFIX: &str = "gts.cf.core.errors.err.v1~";

macro_rules! err_id {
    ($name:ident, $tail:literal, $doc:literal) => {
        #[doc = $doc]
        pub const $name: &str = concat!("gts.cf.core.errors.err.v1~", $tail);
    };
}

err_id!(
    ERR_VALIDATION,
    "cf.oagw.validation.error.v1",
    "400 — request validation failed."
);
err_id!(
    ERR_MISSING_TARGET_HOST,
    "cf.oagw.routing.missing_target_host.v1",
    "400 — `X-OAGW-Target-Host` required to disambiguate the endpoint."
);
err_id!(
    ERR_INVALID_TARGET_HOST,
    "cf.oagw.routing.invalid_target_host.v1",
    "400 — `X-OAGW-Target-Host` is not a bare hostname or IP."
);
err_id!(
    ERR_UNKNOWN_TARGET_HOST,
    "cf.oagw.routing.unknown_target_host.v1",
    "400 — `X-OAGW-Target-Host` matches no configured endpoint."
);
err_id!(
    ERR_AUTH_FAILED,
    "cf.oagw.auth.failed.v1",
    "401 — upstream authentication failed."
);
err_id!(
    ERR_CORS_ORIGIN_NOT_ALLOWED,
    "cf.oagw.cors.origin_not_allowed.v1",
    "403 — request origin is not in `allowed_origins`."
);
err_id!(
    ERR_CORS_METHOD_NOT_ALLOWED,
    "cf.oagw.cors.method_not_allowed.v1",
    "403 — request method is not in `allowed_methods`."
);
err_id!(
    ERR_FORBIDDEN,
    "cf.oagw.forbidden.v1",
    "403 — caller is not permitted."
);
err_id!(
    ERR_ROUTE_NOT_FOUND,
    "cf.oagw.route.not_found.v1",
    "404 — no matching route."
);
err_id!(
    ERR_NOT_FOUND,
    "cf.oagw.not_found.v1",
    "404 — resource does not exist."
);
err_id!(
    ERR_CONFLICT,
    "cf.oagw.conflict.v1",
    "409 — resource conflict."
);
err_id!(
    ERR_PLUGIN_IN_USE,
    "cf.oagw.plugin.in_use.v1",
    "409 — plugin is still referenced."
);
err_id!(
    ERR_PAYLOAD_TOO_LARGE,
    "cf.oagw.payload.too_large.v1",
    "413 — payload above the limit."
);
err_id!(
    ERR_RATE_LIMIT_EXCEEDED,
    "cf.oagw.rate_limit.exceeded.v1",
    "429 — rate limit exceeded."
);
err_id!(
    ERR_SECRET_NOT_FOUND,
    "cf.oagw.secret.not_found.v1",
    "500 — referenced secret missing."
);
err_id!(
    ERR_INTERNAL,
    "cf.oagw.internal.error.v1",
    "500 — unexpected gateway failure."
);
err_id!(
    ERR_PROTOCOL,
    "cf.oagw.protocol.error.v1",
    "502 — protocol-level error."
);
err_id!(
    ERR_DOWNSTREAM,
    "cf.oagw.downstream.error.v1",
    "502 — upstream service error."
);
err_id!(
    ERR_STREAM_ABORTED,
    "cf.oagw.stream.aborted.v1",
    "502 — stream connection aborted."
);
err_id!(
    ERR_LINK_UNAVAILABLE,
    "cf.oagw.link.unavailable.v1",
    "503 — upstream link unavailable."
);
err_id!(
    ERR_CIRCUIT_BREAKER_OPEN,
    "cf.oagw.circuit_breaker.open.v1",
    "503 — circuit breaker is open."
);
err_id!(
    ERR_PLUGIN_NOT_FOUND,
    "cf.oagw.plugin.not_found.v1",
    "503 — plugin could not be resolved."
);
err_id!(
    ERR_CONNECTION_TIMEOUT,
    "cf.oagw.timeout.connection.v1",
    "504 — connect timed out."
);
err_id!(
    ERR_REQUEST_TIMEOUT,
    "cf.oagw.timeout.request.v1",
    "504 — request timed out."
);
err_id!(
    ERR_IDLE_TIMEOUT,
    "cf.oagw.timeout.idle.v1",
    "504 — idle timeout."
);

// ---------------------------------------------------------------------------
// Anonymous instance helpers
// ---------------------------------------------------------------------------

/// Format `uuid` as the anonymous GTS instance of `base_type`.
///
/// `base_type` is expected to end with `~`; a missing separator is added so
/// callers cannot accidentally emit a malformed identifier.
#[must_use]
pub fn anonymous_id(base_type: &str, uuid: Uuid) -> String {
    if base_type.ends_with('~') {
        format!("{base_type}{uuid}")
    } else {
        format!("{base_type}~{uuid}")
    }
}

/// Parse a resource identifier that may be either the anonymous GTS form
/// (`<base_type><uuid>`) or a bare UUID.
///
/// Accepting both is deliberate: the management API emits the GTS form, but
/// clients that stored the raw UUID (or copied it out of a `$select`ed
/// listing) must keep working.
#[must_use]
pub fn parse_resource_id(base_type: &str, raw: &str) -> Option<Uuid> {
    let trimmed = raw.trim();
    let candidate = trimmed
        .strip_prefix("gts://")
        .map_or(trimmed, |rest| rest)
        .trim();
    if let Ok(uuid) = Uuid::parse_str(candidate) {
        return Some(uuid);
    }
    let base = base_type.strip_suffix('~').unwrap_or(base_type);
    // Case-insensitive on the type part; the instance part must be a UUID.
    let (head, tail) = candidate.rsplit_once('~')?;
    if !head.eq_ignore_ascii_case(base) {
        return None;
    }
    Uuid::parse_str(tail).ok()
}

/// Split a plugin reference into its base type and instance part.
#[must_use]
pub fn split_plugin_ref(plugin_ref: &str) -> Option<(&str, &str)> {
    let trimmed = plugin_ref.trim();
    let idx = trimmed.rfind('~')?;
    Some((&trimmed[..=idx], &trimmed[idx + 1..]))
}

/// Extract the UUID of a UUID-backed plugin reference, if any.
#[must_use]
pub fn plugin_ref_uuid(plugin_ref: &str) -> Option<Uuid> {
    let (_, instance) = split_plugin_ref(plugin_ref)?;
    Uuid::parse_str(instance).ok()
}

/// Every GTS identifier OAGW catalogues in the types registry.
#[must_use]
pub fn catalog_plugin_ids() -> Vec<&'static str> {
    vec![
        NOOP_AUTH_PLUGIN_ID,
        APIKEY_AUTH_PLUGIN_ID,
        OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID,
        OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
        BASIC_AUTH_PLUGIN_ID,
        BEARER_AUTH_PLUGIN_ID,
        REQUIRED_HEADERS_GUARD_PLUGIN_ID,
        TIMEOUT_GUARD_PLUGIN_ID,
        CORS_GUARD_PLUGIN_ID,
        REQUEST_ID_TRANSFORM_PLUGIN_ID,
        LOGGING_TRANSFORM_PLUGIN_ID,
        METRICS_TRANSFORM_PLUGIN_ID,
    ]
}

#[cfg(test)]
mod tests {
    use super::{
        APIKEY_AUTH_PLUGIN_ID, AUTH_PLUGIN_TYPE, UPSTREAM_TYPE, anonymous_id, parse_resource_id,
        plugin_ref_uuid, split_plugin_ref,
    };
    use uuid::Uuid;

    #[test]
    fn anonymous_round_trip() {
        let id = Uuid::new_v4();
        let formatted = anonymous_id(UPSTREAM_TYPE, id);
        assert_eq!(formatted, format!("gts.cf.core.oagw.upstream.v1~{id}"));
        assert_eq!(parse_resource_id(UPSTREAM_TYPE, &formatted), Some(id));
    }

    #[test]
    fn bare_uuid_is_accepted() {
        let id = Uuid::new_v4();
        assert_eq!(parse_resource_id(UPSTREAM_TYPE, &id.to_string()), Some(id));
    }

    #[test]
    fn gts_uri_prefix_is_tolerated() {
        let id = Uuid::new_v4();
        let raw = format!("gts://gts.cf.core.oagw.upstream.v1~{id}");
        assert_eq!(parse_resource_id(UPSTREAM_TYPE, &raw), Some(id));
    }

    #[test]
    fn wrong_type_is_rejected() {
        let id = Uuid::new_v4();
        let raw = format!("gts.cf.core.oagw.route.v1~{id}");
        assert_eq!(parse_resource_id(UPSTREAM_TYPE, &raw), None);
    }

    #[test]
    fn garbage_is_rejected() {
        assert_eq!(parse_resource_id(UPSTREAM_TYPE, "not-an-id"), None);
    }

    #[test]
    fn plugin_ref_split() {
        let (base, instance) = split_plugin_ref(APIKEY_AUTH_PLUGIN_ID).expect("splits");
        assert_eq!(base, AUTH_PLUGIN_TYPE);
        assert_eq!(instance, "cf.core.oagw.apikey.v1");
        assert!(plugin_ref_uuid(APIKEY_AUTH_PLUGIN_ID).is_none());

        let uuid = Uuid::new_v4();
        let custom = format!("gts.cf.core.oagw.guard_plugin.v1~{uuid}");
        assert_eq!(plugin_ref_uuid(&custom), Some(uuid));
    }
}
