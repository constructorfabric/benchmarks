//! GTS identifiers owned by OAGW plus the helpers that parse them.
//!
//! Every resource and plugin OAGW exposes is named by a GTS identifier. The
//! constants here are the single source of truth for those strings; the
//! parsing helpers turn the API-layer identifier form
//! (`gts.cf.core.oagw.upstream.v1~{uuid}`) into the UUID the storage layer
//! keys on, and back.

use toolkit_gts::gts_id;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Resource base types
// ---------------------------------------------------------------------------

/// Upstream resource base type.
pub const UPSTREAM_TYPE: &str = gts_id!("cf.core.oagw.upstream.v1~");
/// Route resource base type.
pub const ROUTE_TYPE: &str = gts_id!("cf.core.oagw.route.v1~");
/// Proxy invocation pseudo-resource (permission target).
pub const PROXY_TYPE: &str = gts_id!("cf.core.oagw.proxy.v1~");

// ---------------------------------------------------------------------------
// Protocols
// ---------------------------------------------------------------------------

/// HTTP upstream protocol.
pub const PROTOCOL_HTTP: &str = gts_id!("cf.core.oagw.protocol.v1~cf.core.oagw.http.v1");
/// gRPC upstream protocol (catalogued; proxy path is phase 3).
pub const PROTOCOL_GRPC: &str = gts_id!("cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1");

// ---------------------------------------------------------------------------
// Plugin base types
// ---------------------------------------------------------------------------

/// Auth plugin base type.
pub const AUTH_PLUGIN_TYPE: &str = gts_id!("cf.core.oagw.auth_plugin.v1~");
/// Guard plugin base type.
pub const GUARD_PLUGIN_TYPE: &str = gts_id!("cf.core.oagw.guard_plugin.v1~");
/// Transform plugin base type.
pub const TRANSFORM_PLUGIN_TYPE: &str = gts_id!("cf.core.oagw.transform_plugin.v1~");

// ---------------------------------------------------------------------------
// Built-in auth plugins
// ---------------------------------------------------------------------------

/// No-op auth plugin (injects nothing).
pub const NOOP_AUTH_PLUGIN_ID: &str = gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1");
/// API-key auth plugin (header or query injection).
pub const APIKEY_AUTH_PLUGIN_ID: &str =
    gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1");
/// OAuth2 client-credentials plugin, `Form` client-auth method.
pub const OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID: &str =
    gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1");
/// OAuth2 client-credentials plugin, `Basic` client-auth method.
pub const OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID: &str =
    gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1");
/// Catalog-only: HTTP Basic auth. No backing `AuthPlugin` implementation.
pub const BASIC_AUTH_PLUGIN_ID: &str = gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1");
/// Catalog-only: Bearer token auth. No backing `AuthPlugin` implementation.
pub const BEARER_AUTH_PLUGIN_ID: &str =
    gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1");

// ---------------------------------------------------------------------------
// Built-in guard plugins
// ---------------------------------------------------------------------------

/// Required-headers guard plugin — the only `plugins`-bindable guard.
pub const REQUIRED_HEADERS_GUARD_PLUGIN_ID: &str =
    gts_id!("cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1");
/// Catalog-only: request timeout. Implemented as core Data Plane config.
pub const TIMEOUT_GUARD_PLUGIN_ID: &str =
    gts_id!("cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1");
/// Catalog-only: CORS. Implemented as core Data Plane logic via `Upstream.cors`.
pub const CORS_GUARD_PLUGIN_ID: &str = gts_id!("cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1");

// ---------------------------------------------------------------------------
// Built-in transform plugins
// ---------------------------------------------------------------------------

/// `X-Request-ID` propagation transform plugin.
pub const REQUEST_ID_TRANSFORM_PLUGIN_ID: &str =
    gts_id!("cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1");
/// Catalog-only: request/response logging. Core Data Plane instrumentation.
pub const LOGGING_TRANSFORM_PLUGIN_ID: &str =
    gts_id!("cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1");
/// Catalog-only: Prometheus metrics. Core Data Plane instrumentation.
pub const METRICS_TRANSFORM_PLUGIN_ID: &str =
    gts_id!("cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1");

/// Every GTS identifier OAGW catalogues, in the order the types-registry
/// should see them.
pub const CATALOG_PLUGIN_IDS: &[&str] = &[
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
];

/// The three plugin kinds OAGW distinguishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PluginKind {
    /// Credential injection. One per upstream.
    Auth,
    /// Validation / policy enforcement. Can reject a request.
    Guard,
    /// Request / response mutation.
    Transform,
}

impl PluginKind {
    /// Short wire name (`auth` / `guard` / `transform`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Guard => "guard",
            Self::Transform => "transform",
        }
    }

    /// GTS base type for this kind.
    #[must_use]
    pub fn base_type(self) -> &'static str {
        match self {
            Self::Auth => AUTH_PLUGIN_TYPE,
            Self::Guard => GUARD_PLUGIN_TYPE,
            Self::Transform => TRANSFORM_PLUGIN_TYPE,
        }
    }

    /// Parse the short wire name.
    #[must_use]
    pub fn from_str_opt(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auth" => Some(Self::Auth),
            "guard" => Some(Self::Guard),
            "transform" => Some(Self::Transform),
            _ => None,
        }
    }

    /// Recover the kind from a full plugin GTS identifier.
    #[must_use]
    pub fn from_plugin_ref(plugin_ref: &str) -> Option<Self> {
        [Self::Auth, Self::Guard, Self::Transform]
            .into_iter()
            .find(|kind| plugin_ref.starts_with(kind.base_type()))
    }
}

/// Render an anonymous GTS identifier — `{base_type}{uuid}`.
#[must_use]
pub fn anonymous_id(base_type: &str, id: Uuid) -> String {
    format!("{base_type}{id}")
}

/// Accept either a bare UUID or the anonymous GTS form for `base_type`,
/// returning the UUID.
///
/// A GTS identifier whose base type does not match `base_type` is rejected so
/// a route id can never be mistaken for an upstream id.
#[must_use]
pub fn parse_resource_id(base_type: &str, raw: &str) -> Option<Uuid> {
    let raw = raw.trim();
    if let Ok(id) = Uuid::parse_str(raw) {
        return Some(id);
    }
    let rest = raw.strip_prefix(base_type)?;
    Uuid::parse_str(rest).ok()
}

/// Split a plugin reference into its base type and instance part.
#[must_use]
pub fn split_plugin_ref(plugin_ref: &str) -> Option<(&str, &str)> {
    let idx = plugin_ref.rfind('~')?;
    Some((&plugin_ref[..=idx], &plugin_ref[idx + 1..]))
}

/// The UUID a UUID-backed plugin reference carries, if any.
///
/// A named (built-in) reference such as
/// `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1` yields `None`.
#[must_use]
pub fn plugin_ref_uuid(plugin_ref: &str) -> Option<Uuid> {
    let (_, instance) = split_plugin_ref(plugin_ref)?;
    Uuid::parse_str(instance).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_carry_the_documented_shape() {
        assert_eq!(UPSTREAM_TYPE, "gts.cf.core.oagw.upstream.v1~");
        assert_eq!(
            APIKEY_AUTH_PLUGIN_ID,
            "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1"
        );
        assert_eq!(
            PROTOCOL_HTTP,
            "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
        );
    }

    #[test]
    fn resource_ids_round_trip() {
        let id = Uuid::new_v4();
        let gts = anonymous_id(UPSTREAM_TYPE, id);
        assert_eq!(parse_resource_id(UPSTREAM_TYPE, &gts), Some(id));
        assert_eq!(parse_resource_id(UPSTREAM_TYPE, &id.to_string()), Some(id));
        // Cross-type identifiers are refused.
        assert_eq!(parse_resource_id(ROUTE_TYPE, &gts), None);
    }

    #[test]
    fn plugin_refs_split_on_the_last_tilde() {
        let uuid = Uuid::new_v4();
        let custom = format!("{GUARD_PLUGIN_TYPE}{uuid}");
        assert_eq!(plugin_ref_uuid(&custom), Some(uuid));
        assert_eq!(plugin_ref_uuid(REQUIRED_HEADERS_GUARD_PLUGIN_ID), None);
        assert_eq!(
            PluginKind::from_plugin_ref(REQUIRED_HEADERS_GUARD_PLUGIN_ID),
            Some(PluginKind::Guard)
        );
        assert_eq!(
            PluginKind::from_plugin_ref(&custom),
            Some(PluginKind::Guard)
        );
    }

    #[test]
    fn plugin_kind_wire_names() {
        assert_eq!(PluginKind::from_str_opt("Auth"), Some(PluginKind::Auth));
        assert_eq!(PluginKind::from_str_opt("nope"), None);
        assert_eq!(PluginKind::Transform.as_str(), "transform");
    }
}
