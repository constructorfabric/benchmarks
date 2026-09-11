//! The gear's GTS catalog.
//!
//! Every identifier the `oagw` gear owns lives here, in one table: the three
//! plugin *type schemas* the gear defines, the well-known plugin instances that
//! resolve through their registries, and the catalog-only identifiers that
//! exist for the types-registry catalog but deliberately resolve to nothing.
//!
//! The catalog is the single source the plugin registries and the control
//! plane's reference validation are checked against, so a plugin id cannot
//! drift between `docs/DESIGN.md` and the running gear.


/// One entry of the gear's GTS catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogEntry {
    /// The full GTS identifier.
    pub gts_id: &'static str,
    /// The plugin family the identifier belongs to.
    pub family: PluginFamily,
    /// What the identifier does at runtime.
    pub behaviour: &'static str,
    /// Whether an implementation backs the identifier.
    pub bindable: bool,
}

/// The three plugin families, plus the resource types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginFamily {
    /// Credential injection.
    Auth,
    /// Policy enforcement.
    Guard,
    /// Request/response mutation.
    Transform,
    /// A resource type, not a plugin.
    Resource,
}

impl PluginFamily {
    /// The type-schema identifier of this family.
    #[must_use]
    pub const fn type_id(self) -> &'static str {
        match self {
            Self::Auth => crate::domain::gts_helpers::AUTH_PLUGIN_TYPE_ID,
            Self::Guard => crate::domain::gts_helpers::GUARD_PLUGIN_TYPE_ID,
            Self::Transform => crate::domain::gts_helpers::TRANSFORM_PLUGIN_TYPE_ID,
            Self::Resource => crate::domain::gts_helpers::UPSTREAM_TYPE_ID,
        }
    }
}

/// Every identifier the gear declares, in catalog order.
pub const CATALOG: &[CatalogEntry] = &[
    CatalogEntry {
        gts_id: crate::domain::gts_helpers::AUTH_PLUGIN_NOOP,
        family: PluginFamily::Auth,
        behaviour: "no authentication",
        bindable: true,
    },
    CatalogEntry {
        gts_id: crate::domain::gts_helpers::AUTH_PLUGIN_APIKEY,
        family: PluginFamily::Auth,
        behaviour: "API key injection into a header or the query string",
        bindable: true,
    },
    CatalogEntry {
        gts_id: crate::domain::gts_helpers::AUTH_PLUGIN_OAUTH2_CLIENT_CRED,
        family: PluginFamily::Auth,
        behaviour: "OAuth2 client credentials, form client auth",
        bindable: true,
    },
    CatalogEntry {
        gts_id: crate::domain::gts_helpers::AUTH_PLUGIN_OAUTH2_CLIENT_CRED_BASIC,
        family: PluginFamily::Auth,
        behaviour: "OAuth2 client credentials, basic client auth",
        bindable: true,
    },
    CatalogEntry {
        gts_id: crate::domain::gts_helpers::CATALOG_ONLY_BASIC,
        family: PluginFamily::Auth,
        behaviour: "HTTP Basic; no runtime implementation",
        bindable: false,
    },
    CatalogEntry {
        gts_id: crate::domain::gts_helpers::CATALOG_ONLY_BEARER,
        family: PluginFamily::Auth,
        behaviour: "static bearer token; no runtime implementation",
        bindable: false,
    },
    CatalogEntry {
        gts_id: crate::domain::gts_helpers::REQUIRED_HEADERS_GUARD_PLUGIN_ID,
        family: PluginFamily::Guard,
        behaviour: "required request/response header presence",
        bindable: true,
    },
    CatalogEntry {
        gts_id: crate::domain::gts_helpers::CATALOG_ONLY_TIMEOUT,
        family: PluginFamily::Guard,
        behaviour: "gear-level request timeout, not a plugin",
        bindable: false,
    },
    CatalogEntry {
        gts_id: crate::domain::gts_helpers::CATALOG_ONLY_CORS,
        family: PluginFamily::Guard,
        behaviour: "the `cors` field, not a guard plugin",
        bindable: false,
    },
    CatalogEntry {
        gts_id: crate::domain::gts_helpers::REQUEST_ID_TRANSFORM_PLUGIN_ID,
        family: PluginFamily::Transform,
        behaviour: "X-Request-ID propagation",
        bindable: true,
    },
    CatalogEntry {
        gts_id: crate::domain::gts_helpers::CATALOG_ONLY_LOGGING,
        family: PluginFamily::Transform,
        behaviour: "core instrumentation, not a transform plugin",
        bindable: false,
    },
    CatalogEntry {
        gts_id: crate::domain::gts_helpers::CATALOG_ONLY_METRICS,
        family: PluginFamily::Transform,
        behaviour: "core instrumentation, not a transform plugin",
        bindable: false,
    },
];

/// The gear's resource types: upstream, route and the three plugin families.
pub const RESOURCE_TYPES: &[(&str, PluginFamily)] = &[
    (
        crate::domain::gts_helpers::UPSTREAM_TYPE_ID,
        PluginFamily::Resource,
    ),
    (
        crate::domain::gts_helpers::ROUTE_TYPE_ID,
        PluginFamily::Resource,
    ),
    (
        crate::domain::gts_helpers::AUTH_PLUGIN_TYPE_ID,
        PluginFamily::Auth,
    ),
    (
        crate::domain::gts_helpers::GUARD_PLUGIN_TYPE_ID,
        PluginFamily::Guard,
    ),
    (
        crate::domain::gts_helpers::TRANSFORM_PLUGIN_TYPE_ID,
        PluginFamily::Transform,
    ),
];

/// The catalog entries a `plugins.items[]` reference may bind to.
#[must_use]
pub fn bindable() -> Vec<&'static str> {
    CATALOG
        .iter()
        .filter(|entry| entry.bindable)
        .map(|entry| entry.gts_id)
        .collect()
}

/// The catalog entries that exist only to be cataloged.
#[must_use]
pub fn catalog_only() -> Vec<&'static str> {
    CATALOG
        .iter()
        .filter(|entry| !entry.bindable)
        .map(|entry| entry.gts_id)
        .collect()
}

/// Renders the catalog as the log line the gear emits on start-up.
#[must_use]
pub fn describe() -> String {
    let mut out = String::from("oagw GTS catalog:");
    for entry in CATALOG {
        out.push_str("\n  - ");
        out.push_str(entry.gts_id);
        out.push_str(if entry.bindable {
            " (bindable)"
        } else {
            " (catalog only)"
        });
        out.push_str(" \u{2014} ");
        out.push_str(entry.behaviour);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalog_matches_the_registries() {
        assert_eq!(bindable().len(), 6);
        assert_eq!(catalog_only().len(), 6);
        assert!(bindable().contains(&crate::domain::gts_helpers::AUTH_PLUGIN_APIKEY));
        assert!(catalog_only().contains(&crate::domain::gts_helpers::CATALOG_ONLY_BASIC));
    }

    #[test]
    fn every_catalog_id_is_in_its_family() {
        for entry in CATALOG {
            assert!(
                entry.gts_id.starts_with(entry.family.type_id()),
                "{} must be under {}",
                entry.gts_id,
                entry.family.type_id()
            );
        }
    }

    #[test]
    fn catalog_only_ids_are_rejected_by_validation() {
        for id in catalog_only() {
            assert!(
                crate::domain::services::management::validate_auth_plugin_reference(id).is_err()
                    || crate::domain::services::management::validate_bindable_plugin_reference(id)
                        .is_err(),
                "{id} must not be bindable"
            );
        }
    }

    #[test]
    fn the_description_names_every_entry() {
        let text = describe();
        for entry in CATALOG {
            assert!(text.contains(entry.gts_id), "{} missing", entry.gts_id);
        }
    }
}
