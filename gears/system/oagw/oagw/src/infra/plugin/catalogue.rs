//! Plugin catalogue: built-in identifiers and their definitions.
//!
//! Built-in plugins have no stored definition; they are catalogued here so
//! `GET /oagw/v1/plugins` can enumerate them alongside tenant-defined custom
//! plugins (`DESIGN.md` "Plugin Identification Model").

use uuid::Uuid;

use crate::domain::model::{
    CustomPlugin, auth_plugin_ids as auth, guard_plugin_ids as guard,
    transform_plugin_ids as transform,
};

/// Every built-in plugin identifier, in catalogue order.
pub const BUILTIN_PLUGIN_IDS: [&str; 12] = [
    auth::NOOP,
    auth::APIKEY,
    auth::OAUTH2_CC,
    auth::OAUTH2_CC_BASIC,
    auth::BASIC,
    auth::BEARER,
    guard::REQUIRED_HEADERS,
    guard::TIMEOUT,
    guard::CORS,
    transform::REQUEST_ID,
    transform::LOGGING,
    transform::METRICS,
];

/// The plugin kind declared by a GTS identifier (`auth`, `guard`, or
/// `transform`).
#[must_use]
pub fn plugin_kind(identifier: &str) -> &'static str {
    if identifier.contains("auth_plugin") {
        "auth"
    } else if identifier.contains("guard_plugin") {
        "guard"
    } else {
        "transform"
    }
}

/// The short name of a built-in plugin.
///
/// The reduction lives in [`crate::domain::model::plugin_short_name`] so the
/// catalogue, the validator, and the registries agree on how a versioned
/// identifier resolves.
#[must_use]
fn short_name(identifier: &str) -> String {
    crate::domain::model::plugin_short_name(identifier)
}

/// A catalogued definition for a built-in plugin.
///
/// Built-in plugins are named (not UUID-backed), so their `id` is the nil
/// UUID and they are never garbage collected.
#[must_use]
pub fn builtin_definition(identifier: &str) -> CustomPlugin {
    CustomPlugin {
        id: Uuid::nil(),
        tenant_id: Uuid::nil(),
        plugin_type: plugin_kind(identifier).to_owned(),
        name: short_name(identifier),
        source_code: String::new(),
        config_schema: serde_json::Value::Object(serde_json::Map::new()),
        gc_eligible: false,
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn kinds_are_derived_from_the_identifier() {
        assert_eq!(plugin_kind(auth::NOOP), "auth");
        assert_eq!(plugin_kind(guard::REQUIRED_HEADERS), "guard");
        assert_eq!(plugin_kind(transform::REQUEST_ID), "transform");
    }

    #[test]
    fn every_builtin_gets_a_definition() {
        for identifier in BUILTIN_PLUGIN_IDS {
            let definition = builtin_definition(identifier);
            assert!(!definition.name.is_empty(), "{identifier} must have a name");
        }
    }

    #[test]
    fn short_names_drop_the_trailing_version_segment() {
        // A full GTS identifier: the instance form ends in the version, which
        // is not part of the name (`PRD.md` "Built-in Plugins").
        assert_eq!(short_name(auth::NOOP), "noop");
        assert_eq!(short_name(auth::APIKEY), "apikey");
        assert_eq!(short_name(auth::OAUTH2_CC), "oauth2_client_cred");
        assert_eq!(
            short_name(auth::OAUTH2_CC_BASIC),
            "oauth2_client_cred_basic"
        );
        assert_eq!(short_name(guard::REQUIRED_HEADERS), "required_headers");
        assert_eq!(short_name(guard::TIMEOUT), "timeout");
        assert_eq!(short_name(transform::REQUEST_ID), "request_id");
        // An identifier spelled without a version keeps its last segment, and a
        // lone version segment is still a name rather than an empty string.
        assert_eq!(short_name("cf.core.oagw.apikey"), "apikey");
        assert_eq!(short_name("v1"), "v1");
        // Any version token, not only `v1`, is dropped: the reducer is shared
        // with the validator, so a `v2` spelling resolves to the same name.
        assert_eq!(
            short_name("cf.core.oagw.required_headers.v2"),
            "required_headers"
        );
    }

    #[test]
    fn builtin_names_are_distinct() {
        let mut names: Vec<String> = BUILTIN_PLUGIN_IDS
            .iter()
            .map(|identifier| builtin_definition(identifier).name)
            .collect();
        assert_eq!(
            names.len(),
            BUILTIN_PLUGIN_IDS.len(),
            "every built-in is named"
        );
        names.sort();
        names.dedup();
        assert_eq!(
            names.len(),
            BUILTIN_PLUGIN_IDS.len(),
            "short names are unique"
        );
    }
}
