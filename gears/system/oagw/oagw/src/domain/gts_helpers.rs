//! GTS identifiers owned by the OAGW gear plus the parsing helpers used to
//! move between the anonymous-identifier form used in API paths
//! (`gts.cf.core.oagw.upstream.v1~{uuid}`) and the bare UUID used in bodies.

use uuid::Uuid;

// --- Resource base types -------------------------------------------------

pub const UPSTREAM_TYPE: &str = "gts.cf.core.oagw.upstream.v1~";
pub const ROUTE_TYPE: &str = "gts.cf.core.oagw.route.v1~";
pub const PROXY_TYPE: &str = "gts.cf.core.oagw.proxy.v1~";

pub const AUTH_PLUGIN_TYPE: &str = "gts.cf.core.oagw.auth_plugin.v1~";
pub const GUARD_PLUGIN_TYPE: &str = "gts.cf.core.oagw.guard_plugin.v1~";
pub const TRANSFORM_PLUGIN_TYPE: &str = "gts.cf.core.oagw.transform_plugin.v1~";

// --- Protocols -----------------------------------------------------------

pub const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
pub const PROTOCOL_GRPC: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";

// --- Built-in auth plugins ----------------------------------------------

pub const NOOP_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1";
pub const APIKEY_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
pub const OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";
pub const OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1";
/// Catalog-only: cataloged in the types-registry, no backing implementation.
pub const BASIC_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1";
/// Catalog-only: cataloged in the types-registry, no backing implementation.
pub const BEARER_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1";

// --- Built-in guard plugins ---------------------------------------------

pub const REQUIRED_HEADERS_GUARD_PLUGIN_ID: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
/// Catalog-only: request timeout is core Data Plane configuration.
pub const TIMEOUT_GUARD_PLUGIN_ID: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1";
/// Catalog-only: CORS is core Data Plane logic driven by `Upstream.cors`.
pub const CORS_GUARD_PLUGIN_ID: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1";

// --- Built-in transform plugins -----------------------------------------

pub const REQUEST_ID_TRANSFORM_PLUGIN_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";
/// Catalog-only: logging is core Data Plane instrumentation.
pub const LOGGING_TRANSFORM_PLUGIN_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1";
/// Catalog-only: metrics collection is core Data Plane instrumentation.
pub const METRICS_TRANSFORM_PLUGIN_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1";

/// Format an anonymous GTS identifier: `{base_type}{uuid}`.
///
/// `base_type` already ends with `~`, so the instance part is appended
/// directly.
#[must_use]
pub fn anonymous_id(base_type: &str, id: Uuid) -> String {
    format!("{base_type}{id}")
}

/// Accept either a bare UUID or an anonymous GTS identifier whose base type
/// matches `base_type`, returning the UUID.
///
/// Path parameters are documented as anonymous GTS identifiers while the
/// response bodies carry bare UUIDs (see `schemas/*.schema.json`), so both
/// spellings have to round-trip.
#[must_use]
pub fn parse_resource_id(raw: &str, base_type: &str) -> Option<Uuid> {
    let raw = raw.trim();
    if let Ok(id) = Uuid::parse_str(raw) {
        return Some(id);
    }
    let rest = raw.strip_prefix(base_type)?;
    Uuid::parse_str(rest).ok()
}

/// Split a plugin GTS identifier into `(base_type, instance)`.
#[must_use]
pub fn split_gts(raw: &str) -> Option<(&str, &str)> {
    let idx = raw.rfind('~')?;
    Some((&raw[..=idx], &raw[idx + 1..]))
}

/// Resolve a plugin reference to its UUID when the instance part is a UUID.
///
/// Named (built-in) plugins return `None` — they are resolved through the
/// in-process registry instead (DESIGN "Resolution Algorithm").
#[must_use]
pub fn plugin_ref_uuid(plugin_ref: &str) -> Option<Uuid> {
    if let Ok(id) = Uuid::parse_str(plugin_ref) {
        return Some(id);
    }
    let (_, instance) = split_gts(plugin_ref)?;
    Uuid::parse_str(instance).ok()
}

/// Every GTS identifier the gear publishes to the types-registry catalog.
#[must_use]
pub fn catalog_ids() -> Vec<&'static str> {
    vec![
        UPSTREAM_TYPE,
        ROUTE_TYPE,
        PROXY_TYPE,
        AUTH_PLUGIN_TYPE,
        GUARD_PLUGIN_TYPE,
        TRANSFORM_PLUGIN_TYPE,
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
    use super::*;

    #[test]
    fn resource_ids_round_trip_in_both_spellings() {
        let id = Uuid::new_v4();
        let gts = anonymous_id(UPSTREAM_TYPE, id);
        assert_eq!(gts, format!("gts.cf.core.oagw.upstream.v1~{id}"));
        assert_eq!(parse_resource_id(&gts, UPSTREAM_TYPE), Some(id));
        assert_eq!(parse_resource_id(&id.to_string(), UPSTREAM_TYPE), Some(id));
    }

    #[test]
    fn resource_id_rejects_a_foreign_base_type() {
        let id = Uuid::new_v4();
        let gts = anonymous_id(ROUTE_TYPE, id);
        assert_eq!(parse_resource_id(&gts, UPSTREAM_TYPE), None);
    }

    #[test]
    fn named_plugin_refs_have_no_uuid() {
        assert_eq!(plugin_ref_uuid(APIKEY_AUTH_PLUGIN_ID), None);
        let id = Uuid::new_v4();
        assert_eq!(
            plugin_ref_uuid(&anonymous_id(GUARD_PLUGIN_TYPE, id)),
            Some(id)
        );
        assert_eq!(plugin_ref_uuid(&id.to_string()), Some(id));
    }

    #[test]
    fn split_gts_takes_the_last_tilde() {
        let (base, instance) = split_gts(APIKEY_AUTH_PLUGIN_ID).expect("split");
        assert_eq!(base, AUTH_PLUGIN_TYPE);
        assert_eq!(instance, "cf.core.oagw.apikey.v1");
    }
}
