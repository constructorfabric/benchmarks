//! GTS plugin identifier model (DoD `cpt-cf-oagw-dod-plugin-system-identification`,
//! algorithm `cpt-cf-oagw-algo-plugin-system-resolve-gts`).
//!
//! Plugin references are canonical GTS identifiers of the form
//! `gts.cf.core.oagw.{type}_plugin.v1~{instance}`, where `{type}` is one of
//! `auth_plugin`, `guard_plugin`, `transform_plugin` (step
//! `inst-ps-gts-parse`).  The instance segment names a concrete plugin
//! (`cf.core.oagw.apikey.v1`, ...).  This module owns the constants for every
//! built-in and catalog-only identifier and the parser that classifies a
//! `plugin_ref` into its [`PluginType`] and instance segments.

use crate::domain::entity::plugin::PluginType;

/// Type-id prefix shared by all OAGW plugin types.
pub const TYPE_ID_PREFIX: &str = "gts.cf.core.oagw.";

/// Instance (type) prefix shared by all OAGW plugin instances.
pub const INSTANCE_PREFIX: &str = "cf.core.oagw.";

// --- auth plugin identifiers (`cpt-cf-oagw-dod-plugin-system-builtins`) ---

/// `noop` — no authentication performed.
pub const NOOP_AUTH: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1";
/// `apikey` — header/query API-key injection.
pub const APIKEY_AUTH: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
/// `oauth2_client_cred` — OAuth2 client-credentials with `Form` client auth.
pub const OAUTH2_CLIENT_CRED: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";
/// `oauth2_client_cred_basic` — OAuth2 client-credentials with `Basic` client auth.
pub const OAUTH2_CLIENT_CRED_BASIC: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1";

// --- guard plugin identifiers ---

/// `required_headers` — stateless presence checks (ADR 0009).
pub const REQUIRED_HEADERS_GUARD: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";

// --- transform plugin identifiers ---

/// `request_id` — `X-Request-ID` propagation.
pub const REQUEST_ID_TRANSFORM: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";

// --- catalog-only identifiers (DoD `cpt-cf-oagw-dod-plugin-system-catalog-only`) ---
//
// Registered in the types registry for cataloging purposes only; none of them
// resolves through a plugin registry — binding one yields
// `PluginNotFound` (503) (step `inst-ps-gts-catalog-only`).

/// `basic` — reserved, catalog-only (no `AuthPlugin` implementation).
pub const BASIC_AUTH: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1";
/// `bearer` — reserved, catalog-only.
pub const BEARER_AUTH: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1";
/// `timeout` — core Data Plane logic, catalog-only guard identifier.
pub const TIMEOUT_GUARD: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1";
/// `cors` — core Data Plane logic, catalog-only guard identifier.
pub const CORS_GUARD: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1";
/// `logging` — core instrumentation, catalog-only transform identifier.
pub const LOGGING_TRANSFORM: &str = "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1";
/// `metrics` — core instrumentation, catalog-only transform identifier.
pub const METRICS_TRANSFORM: &str = "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1";

/// The six catalog-only identifiers (DoD
/// `cpt-cf-oagw-dod-plugin-system-catalog-only`).
pub const CATALOG_ONLY: &[&str] = &[
    BASIC_AUTH,
    BEARER_AUTH,
    TIMEOUT_GUARD,
    CORS_GUARD,
    LOGGING_TRANSFORM,
    METRICS_TRANSFORM,
];

/// A parsed canonical GTS plugin reference.
///
/// Splits a `plugin_ref` into its plugin type and instance segments (step
/// `inst-ps-gts-parse`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GtsPluginRef {
    /// The full canonical identifier as carried by the binding
    /// (`plugin_ref`).
    pub full: String,
    /// The plugin type segment (`auth_plugin` | `guard_plugin` |
    /// `transform_plugin`).
    pub plugin_type: PluginType,
    /// The instance segment, e.g. `cf.core.oagw.apikey.v1`.
    pub instance: String,
}

impl GtsPluginRef {
    /// Parses a raw `plugin_ref` into its type and instance segments.
    ///
    /// The type must be one of `auth_plugin` / `guard_plugin` /
    /// `transform_plugin` and the instance must carry the
    /// `cf.core.oagw.` prefix.
    ///
    /// # Errors
    /// Returns `None` for any malformed reference (wrong prefix, missing
    /// `~` separator, unknown type segment, or empty instance).
    pub fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        if !raw.starts_with(TYPE_ID_PREFIX) {
            return None;
        }
        let (type_id, instance) = raw.split_once('~')?;
        let type_segment = type_id.strip_prefix(TYPE_ID_PREFIX)?.strip_suffix(".v1")?;
        let plugin_type = match type_segment {
            "auth_plugin" => PluginType::Auth,
            "guard_plugin" => PluginType::Guard,
            "transform_plugin" => PluginType::Transform,
            _ => return None,
        };
        if !instance.starts_with(INSTANCE_PREFIX) || instance.len() == INSTANCE_PREFIX.len() {
            return None;
        }
        Some(Self {
            full: raw.to_owned(),
            plugin_type,
            instance: instance.to_owned(),
        })
    }

    /// Whether this reference names a catalog-only identifier (DoD
    /// `cpt-cf-oagw-dod-plugin-system-catalog-only`, step
    /// `inst-ps-gts-catalog-only`).
    #[must_use]
    pub fn is_catalog_only(&self) -> bool {
        CATALOG_ONLY.contains(&self.full.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_builtin_refs_into_their_type_and_instance() {
        let cases = [
            (NOOP_AUTH, PluginType::Auth, "cf.core.oagw.noop.v1"),
            (APIKEY_AUTH, PluginType::Auth, "cf.core.oagw.apikey.v1"),
            (
                OAUTH2_CLIENT_CRED,
                PluginType::Auth,
                "cf.core.oagw.oauth2_client_cred.v1",
            ),
            (
                OAUTH2_CLIENT_CRED_BASIC,
                PluginType::Auth,
                "cf.core.oagw.oauth2_client_cred_basic.v1",
            ),
            (
                REQUIRED_HEADERS_GUARD,
                PluginType::Guard,
                "cf.core.oagw.required_headers.v1",
            ),
            (
                REQUEST_ID_TRANSFORM,
                PluginType::Transform,
                "cf.core.oagw.request_id.v1",
            ),
        ];
        for (full, plugin_type, instance) in cases {
            let parsed = GtsPluginRef::parse(full).expect("parse");
            assert_eq!(parsed.plugin_type, plugin_type);
            assert_eq!(parsed.instance, instance);
            assert_eq!(parsed.full, full);
            assert!(!parsed.is_catalog_only());
        }
    }

    #[test]
    fn catalog_only_identifiers_are_detected() {
        for id in CATALOG_ONLY {
            let parsed = GtsPluginRef::parse(id).expect("parse");
            assert!(parsed.is_catalog_only(), "{id} must be catalog-only");
        }
        assert_eq!(CATALOG_ONLY.len(), 6);
    }

    #[test]
    fn malformed_refs_are_rejected() {
        for bad in [
            "",
            "gts.cf.core.oagw",
            "gts.cf.core.oagw.auth_plugin.v1",
            "gts.cf.core.oagw.auth_plugin.v1~",
            "gts.cf.core.oagw.other_plugin.v1~cf.core.oagw.x.v1",
            "gts.cf.core.oagw.auth_plugin.v2~cf.core.oagw.noop.v1",
            "gts.cf.core.oagw.auth_plugin.v1~nope",
            "http://evil",
        ] {
            assert!(
                GtsPluginRef::parse(bad).is_none(),
                "{bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn custom_uuid_backed_refs_parse_by_gts_shape_only() {
        // A custom plugin is referenced by its full GTS type id; the UUID that
        // identifies the `oagw_plugin` row rides in `plugin_uuid`, not in the
        // ref.  Type must still be one of the three plugin types.
        let custom = "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.redact_pii.v1";
        let parsed = GtsPluginRef::parse(custom).expect("custom ref is a valid GTS id");
        assert_eq!(parsed.plugin_type, PluginType::Transform);
        assert_eq!(parsed.instance, "cf.core.oagw.redact_pii.v1");
    }
}
