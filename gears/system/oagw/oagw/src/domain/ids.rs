//! GTS identifier helpers and plugin/protocol constants.
//!
//! OAGW identifies resources with anonymous GTS identifiers of the form
//! `gts.cf.core.oagw.{kind}.v1~{uuid}` and named plugins with
//! `gts.cf.core.oagw.{kind}_plugin.v1~cf.core.oagw.{name}.v1`
//! (DESIGN §3.1 "Plugin Identification Model").

use uuid::Uuid;

/// Prefix shared by all OAGW resource/plugin GTS identifiers.
pub const OAGW_GTS_PREFIX: &str = "gts.cf.core.oagw";

/// Base type for upstream instances.
pub const UPSTREAM_TYPE: &str = "gts.cf.core.oagw.upstream.v1";
/// Base type for route instances.
pub const ROUTE_TYPE: &str = "gts.cf.core.oagw.route.v1";

/// Auth plugin identifiers (resolvable via [`crate::infra::plugins::PluginRegistry`]).
pub mod auth {
    pub const NOOP: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1";
    pub const APIKEY: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
    pub const OAUTH2_CLIENT_CRED: &str =
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";
    pub const OAUTH2_CLIENT_CRED_BASIC: &str =
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1";
    /// Catalog-only — no backing implementation (using it fails with
    /// "unknown auth plugin").
    pub const BASIC: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1";
    pub const BEARER: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1";
}

/// Guard plugin identifiers.
pub mod guard {
    pub const REQUIRED_HEADERS: &str =
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
    /// Catalog-only — timeout enforcement is core Data Plane logic.
    pub const TIMEOUT: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1";
    /// Catalog-only — CORS is core Data Plane logic.
    pub const CORS: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1";
}

/// Transform plugin identifiers.
pub mod transform {
    pub const REQUEST_ID: &str = "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";
    /// Catalog-only — logging is core Data Plane instrumentation.
    pub const LOGGING: &str = "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1";
    /// Catalog-only — metrics are core Data Plane instrumentation.
    pub const METRICS: &str = "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1";
}

/// Parse the instance part of a GTS identifier (everything after the last
/// `~`).
pub fn gts_instance(gts_id: &str) -> Option<&str> {
    gts_id.rsplit_once('~').map(|(_, instance)| instance)
}

/// Normalize a resource identifier that may be either an anonymous GTS id
/// (`gts.cf.core.oagw.{kind}.v1~{uuid}`) or a bare UUID into its UUID part.
///
/// Returns `None` when the value is a GTS id whose type prefix does not
/// match `expected_prefix` (e.g. a route id passed where an upstream id is
/// expected).
pub fn extract_resource_uuid(raw: &str, expected_prefix: &str) -> Option<Uuid> {
    let instance = if let Some(instance) = gts_instance(raw) {
        // Full GTS id: validate the type prefix so ids can't be confounded.
        let prefix = raw.trim_end_matches(['~']).split('~').next().unwrap_or("");
        if expected_prefix != prefix {
            return None;
        }
        instance
    } else {
        raw
    };
    Uuid::parse_str(instance).ok()
}

/// Build the anonymous GTS id for an upstream instance.
pub fn upstream_gts_id(id: Uuid) -> String {
    format!("{UPSTREAM_TYPE}~{id}")
}

/// Build the anonymous GTS id for a route instance.
pub fn route_gts_id(id: Uuid) -> String {
    format!("{ROUTE_TYPE}~{id}")
}

/// Build the anonymous GTS id for a UUID-backed plugin.
pub fn plugin_gts_id(kind: &str, id: Uuid) -> String {
    format!("gts.cf.core.oagw.{kind}_plugin.v1~{id}")
}

/// Split a plugin identifier into `(kind, instance)` where kind is one of
/// `auth`, `guard`, `transform`.
pub fn split_plugin_kind(gts_id: &str) -> Option<(&'static str, &str)> {
    let instance = gts_instance(gts_id)?;
    let kind = if gts_id.starts_with("gts.cf.core.oagw.auth_plugin.v1~") {
        "auth"
    } else if gts_id.starts_with("gts.cf.core.oagw.guard_plugin.v1~") {
        "guard"
    } else if gts_id.starts_with("gts.cf.core.oagw.transform_plugin.v1~") {
        "transform"
    } else {
        return None;
    };
    Some((kind, instance))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn uuid() -> Uuid {
        Uuid::parse_str("12345678-1234-1234-1234-123456789012").unwrap()
    }

    #[test]
    fn extracts_uuid_from_gts_and_bare_forms() {
        let gts = format!("{UPSTREAM_TYPE}~{}", uuid());
        assert_eq!(extract_resource_uuid(&gts, UPSTREAM_TYPE), Some(uuid()));
        assert_eq!(
            extract_resource_uuid(&uuid().to_string(), UPSTREAM_TYPE),
            Some(uuid())
        );
    }

    #[test]
    fn rejects_type_mismatch() {
        let route = format!("{ROUTE_TYPE}~{}", uuid());
        assert_eq!(extract_resource_uuid(&route, UPSTREAM_TYPE), None);
        assert_eq!(extract_resource_uuid("not-a-uuid", UPSTREAM_TYPE), None);
    }

    #[test]
    fn classifies_plugin_ids() {
        assert_eq!(
            split_plugin_kind(auth::APIKEY),
            Some(("auth", "cf.core.oagw.apikey.v1"))
        );
        assert_eq!(
            split_plugin_kind(guard::REQUIRED_HEADERS),
            Some(("guard", "cf.core.oagw.required_headers.v1"))
        );
        assert_eq!(
            split_plugin_kind(transform::REQUEST_ID),
            Some(("transform", "cf.core.oagw.request_id.v1"))
        );
        assert_eq!(split_plugin_kind("bogus"), None);
    }
}
