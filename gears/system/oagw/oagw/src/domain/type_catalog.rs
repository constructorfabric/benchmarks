//! The plugin catalog: which identifiers exist, and which of them have a
//! backing implementation (`PRD.md` § 5.3, `DESIGN.md` § 3.2, `ADR/0002` §
//! "Built-in Plugins").
//!
//! Catalog-only identifiers are registered with the types-registry but are
//! *not* resolvable through a plugin registry, so binding one fails.

use std::collections::HashSet;

use crate::domain::gts_helpers as ids;
use crate::domain::model::PluginKind;

/// A catalog entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogEntry {
    /// Full GTS id of the plugin instance.
    pub gts_id: &'static str,
    /// Plugin family.
    pub kind: PluginKind,
    /// Whether a `AuthPlugin`/`GuardPlugin`/`TransformPlugin` implementation
    /// backs the identifier.
    pub implemented: bool,
}

/// Every plugin identifier the gear knows about.
pub const CATALOG: &[CatalogEntry] = &[
    // Auth
    CatalogEntry {
        gts_id: ids::AUTH_NOOP,
        kind: PluginKind::Auth,
        implemented: true,
    },
    CatalogEntry {
        gts_id: ids::AUTH_APIKEY,
        kind: PluginKind::Auth,
        implemented: true,
    },
    CatalogEntry {
        gts_id: ids::AUTH_OAUTH2,
        kind: PluginKind::Auth,
        implemented: true,
    },
    CatalogEntry {
        gts_id: ids::AUTH_OAUTH2_BASIC,
        kind: PluginKind::Auth,
        implemented: true,
    },
    CatalogEntry {
        gts_id: "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1",
        kind: PluginKind::Auth,
        implemented: false,
    },
    CatalogEntry {
        gts_id: "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1",
        kind: PluginKind::Auth,
        implemented: false,
    },
    // Guard
    CatalogEntry {
        gts_id: ids::GUARD_REQUIRED_HEADERS,
        kind: PluginKind::Guard,
        implemented: true,
    },
    CatalogEntry {
        gts_id: "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1",
        kind: PluginKind::Guard,
        implemented: false,
    },
    CatalogEntry {
        gts_id: "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1",
        kind: PluginKind::Guard,
        implemented: false,
    },
    // Transform
    CatalogEntry {
        gts_id: ids::TRANSFORM_REQUEST_ID,
        kind: PluginKind::Transform,
        implemented: true,
    },
    CatalogEntry {
        gts_id: "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1",
        kind: PluginKind::Transform,
        implemented: false,
    },
    CatalogEntry {
        gts_id: "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1",
        kind: PluginKind::Transform,
        implemented: false,
    },
];

/// Looks a full GTS id up in the catalog.
#[must_use]
pub fn lookup(gts_id: &str) -> Option<&'static CatalogEntry> {
    CATALOG.iter().find(|entry| entry.gts_id == gts_id)
}

/// `true` when the identifier is in the catalog, implemented or not.
#[must_use]
pub fn is_known(gts_id: &str) -> bool {
    lookup(gts_id).is_some()
}

/// `true` when the identifier is in the catalog *and* has an implementation.
#[must_use]
pub fn is_bindable(gts_id: &str) -> bool {
    lookup(gts_id).is_some_and(|entry| entry.implemented)
}

/// Every bindable identifier, as a set for validation contexts.
#[must_use]
pub fn bindable_ids() -> HashSet<String> {
    CATALOG
        .iter()
        .filter(|entry| entry.implemented)
        .map(|entry| entry.gts_id.to_owned())
        .collect()
}

/// Every identifier in the catalog, implemented or not.
#[must_use]
pub fn all_ids() -> HashSet<String> {
    CATALOG
        .iter()
        .map(|entry| entry.gts_id.to_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_only_identifiers_are_known_but_not_bindable() {
        assert!(is_known(
            "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1"
        ));
        assert!(!is_bindable(
            "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1"
        ));
        assert!(!is_known(
            "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.jwt.v1"
        ));
    }

    #[test]
    fn every_built_in_is_bindable() {
        assert!(is_bindable(ids::AUTH_NOOP));
        assert!(is_bindable(ids::AUTH_APIKEY));
        assert!(is_bindable(ids::AUTH_OAUTH2));
        assert!(is_bindable(ids::AUTH_OAUTH2_BASIC));
        assert!(is_bindable(ids::GUARD_REQUIRED_HEADERS));
        assert!(is_bindable(ids::TRANSFORM_REQUEST_ID));
    }

    #[test]
    fn the_catalog_has_twelve_entries() {
        assert_eq!(CATALOG.len(), 12);
        assert_eq!(all_ids().len(), 12);
        assert_eq!(bindable_ids().len(), 6);
    }
}
