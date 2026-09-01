// Created: 2026-08-31 by Constructor Tech
//! Plugin kinds and the built-in plugin catalog (DESIGN §3.2, ADR-0002).
//!
//! This module is the *identity* half of the plugin system: the control plane
//! must be able to tell which `auth.plugin_type` / `plugins.items[]` references
//! are bindable, and the data plane must be able to turn a reference into a GTS
//! id its registries know. The plugin contracts and their built-in
//! implementations live in [`crate::infra::plugin`] (ADR-0002 "Built-in
//! Plugins").

use serde::{Deserialize, Serialize};

/// GTS stem for auth plugins (`...auth_plugin.v1~`).
pub const AUTH_PLUGIN_STEM: &str = "gts.cf.core.oagw.auth_plugin.v1~";
/// GTS stem for guard plugins (`...guard_plugin.v1~`).
pub const GUARD_PLUGIN_STEM: &str = "gts.cf.core.oagw.guard_plugin.v1~";
/// GTS stem for transform plugins (`...transform_plugin.v1~`).
pub const TRANSFORM_PLUGIN_STEM: &str = "gts.cf.core.oagw.transform_plugin.v1~";

/// The three plugin families of DESIGN §3.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PluginKind {
    /// Credential injection; one per upstream.
    Auth,
    /// Validation / policy enforcement; may reject.
    Guard,
    /// Request / response mutation.
    Transform,
}

impl PluginKind {
    /// All kinds, in catalog order.
    #[must_use]
    pub const fn all() -> [PluginKind; 3] {
        [PluginKind::Auth, PluginKind::Guard, PluginKind::Transform]
    }

    /// Wire spelling of the kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            PluginKind::Auth => "auth",
            PluginKind::Guard => "guard",
            PluginKind::Transform => "transform",
        }
    }

    /// GTS type stem, e.g. `gts.cf.core.oagw.auth_plugin.v1~`.
    #[must_use]
    pub const fn gts_stem(self) -> &'static str {
        match self {
            PluginKind::Auth => AUTH_PLUGIN_STEM,
            PluginKind::Guard => GUARD_PLUGIN_STEM,
            PluginKind::Transform => TRANSFORM_PLUGIN_STEM,
        }
    }

    /// GTS type stem of a custom plugin record, trailing `~` included.
    ///
    /// `gts.cf.core.oagw.auth_plugin.v1~` + the instance UUID is the full
    /// [`Plugin::gts_id`](crate::domain::model::Plugin::gts_id), which is what
    /// the registries are keyed by.
    #[must_use]
    pub const fn gts_type(self) -> &'static str {
        self.gts_stem()
    }

    /// Parse a kind name (`auth` / `guard` / `transform`).
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auth" => Some(PluginKind::Auth),
            "guard" => Some(PluginKind::Guard),
            "transform" => Some(PluginKind::Transform),
            _ => None,
        }
    }

    /// Build a named (built-in) plugin GTS id: `..._plugin.v1~cf.core.oagw.{name}.v1`.
    #[must_use]
    pub fn built_in_id(self, name: &str) -> String {
        format!("{}cf.core.oagw.{name}.v1", self.gts_stem())
    }
}

/// A built-in plugin as catalogued for the control plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltInPlugin {
    /// Short name (`noop`, `apikey`, …).
    pub name: &'static str,
    /// Owning kind.
    pub kind: PluginKind,
    /// Whether the plugin has a registry implementation and can be bound.
    pub resolvable: bool,
    /// One-line description.
    pub description: &'static str,
}

/// The built-in plugin catalog (DESIGN §3.2 tables).
pub const BUILT_IN_PLUGINS: &[BuiltInPlugin] = &[
    BuiltInPlugin {
        name: "noop",
        kind: PluginKind::Auth,
        resolvable: true,
        description: "No-op authentication; always succeeds",
    },
    BuiltInPlugin {
        name: "apikey",
        kind: PluginKind::Auth,
        resolvable: true,
        description: "Static API key credential injection",
    },
    BuiltInPlugin {
        name: "oauth2_client_cred",
        kind: PluginKind::Auth,
        resolvable: true,
        description: "OAuth2 client-credentials token injection",
    },
    BuiltInPlugin {
        name: "oauth2_client_cred_basic",
        kind: PluginKind::Auth,
        resolvable: true,
        description: "OAuth2 client credentials with HTTP basic authorisation",
    },
    BuiltInPlugin {
        name: "basic",
        kind: PluginKind::Auth,
        resolvable: false,
        description: "Reserved types-registry identifier; no AuthPlugin implementation",
    },
    BuiltInPlugin {
        name: "bearer",
        kind: PluginKind::Auth,
        resolvable: false,
        description: "Reserved types-registry identifier; no AuthPlugin implementation",
    },
    BuiltInPlugin {
        name: "required_headers",
        kind: PluginKind::Guard,
        resolvable: true,
        description: "Required header enforcement on request and response",
    },
    BuiltInPlugin {
        name: "timeout",
        kind: PluginKind::Guard,
        resolvable: false,
        description: "Core data-plane request timeout; not a GuardPlugin",
    },
    BuiltInPlugin {
        name: "cors",
        kind: PluginKind::Guard,
        resolvable: false,
        description: "Core data-plane CORS handling; not a GuardPlugin",
    },
    BuiltInPlugin {
        name: "request_id",
        kind: PluginKind::Transform,
        resolvable: true,
        description: "X-Request-ID injection and propagation",
    },
    BuiltInPlugin {
        name: "logging",
        kind: PluginKind::Transform,
        resolvable: false,
        description: "Core data-plane instrumentation; not a TransformPlugin",
    },
    BuiltInPlugin {
        name: "metrics",
        kind: PluginKind::Transform,
        resolvable: false,
        description: "Core data-plane instrumentation; not a TransformPlugin",
    },
];

/// Credential scope of a plugin reference (DESIGN §3.2 "Plugin
/// Identification Model").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginRef {
    /// Named built-in plugin (`..._plugin.v1~cf.core.oagw.{name}.v1`).
    BuiltIn {
        /// Owning kind.
        kind: PluginKind,
        /// Short name.
        name: String,
        /// Full GTS identifier as sent by the caller.
        raw: String,
        /// Whether a registry implementation exists.
        resolvable: bool,
    },
    /// Tenant-defined custom plugin (`..._plugin.v1~{uuid}`, or a bare UUID).
    Custom {
        /// Owning kind; `None` when the reference carried no GTS stem (a bare
        /// UUID is accepted as a kind-agnostic custom reference, see
        /// `upstream.v1.schema.json` `plugins.items[]`).
        kind: Option<PluginKind>,
        /// Plugin instance UUID.
        id: uuid::Uuid,
        /// Full GTS identifier (or bare UUID) as sent by the caller.
        raw: String,
    },
    /// Anything the control plane cannot classify.
    ///
    /// Kept as a variant (rather than an error) so that slice 2 can still
    /// forward opaque references to the registries.
    Unrecognised(String),
}

impl PluginRef {
    /// Classify a plugin reference string.
    #[must_use]
    pub fn parse(raw: &str) -> PluginRef {
        for kind in PluginKind::all() {
            let Some(rest) = raw.strip_prefix(kind.gts_stem()) else {
                continue;
            };
            // Named built-ins carry a version suffix:
            // `..._plugin.v1~cf.core.oagw.{name}.v1`.
            if let Some(tail) = rest.strip_prefix("cf.core.oagw.") {
                let (name, version) = match tail.split_once('.') {
                    Some((name, version)) => (name, version),
                    None => (tail, ""),
                };
                if !name.is_empty() && (version.is_empty() || version.starts_with('v')) {
                    return PluginRef::BuiltIn {
                        kind,
                        name: name.to_owned(),
                        raw: raw.to_owned(),
                        resolvable: lookup_built_in(kind, name)
                            .is_some_and(|plugin| plugin.resolvable),
                    };
                }
            }
            if let Ok(id) = rest.parse::<uuid::Uuid>() {
                return PluginRef::Custom {
                    kind: Some(kind),
                    id,
                    raw: raw.to_owned(),
                };
            }
        }
        // A bare UUID is a legal custom-plugin reference (the upstream schema
        // spells `plugins.items[]` entries as `oneOf` GTS id / UUID). The kind
        // is unknown, so the stored record decides.
        if let Ok(id) = raw.trim().parse::<uuid::Uuid>() {
            return PluginRef::Custom {
                kind: None,
                id,
                raw: raw.to_owned(),
            };
        }
        PluginRef::Unrecognised(raw.to_owned())
    }

    /// Identifier as stored in the binding table.
    #[must_use]
    pub fn raw(&self) -> &str {
        match self {
            PluginRef::BuiltIn { raw, .. }
            | PluginRef::Custom { raw, .. }
            | PluginRef::Unrecognised(raw) => raw,
        }
    }

    /// Owning kind declared by the reference, when its GTS stem carries one.
    #[must_use]
    pub const fn kind(&self) -> Option<PluginKind> {
        match self {
            PluginRef::BuiltIn { kind, .. } => Some(*kind),
            PluginRef::Custom { kind, .. } => *kind,
            PluginRef::Unrecognised(_) => None,
        }
    }

    /// Plugin instance UUID, custom plugins only.
    #[must_use]
    pub const fn custom_id(&self) -> Option<uuid::Uuid> {
        match self {
            PluginRef::Custom { id, .. } => Some(*id),
            PluginRef::BuiltIn { .. } | PluginRef::Unrecognised(_) => None,
        }
    }

    /// Whether the reference names a built-in plugin that can be bound.
    #[must_use]
    pub const fn is_bindable_built_in(&self) -> bool {
        matches!(
            self,
            PluginRef::BuiltIn {
                resolvable: true,
                ..
            }
        )
    }
}

/// Look up a built-in plugin by kind and short name.
#[must_use]
pub fn lookup_built_in(kind: PluginKind, name: &str) -> Option<&'static BuiltInPlugin> {
    BUILT_IN_PLUGINS
        .iter()
        .find(|plugin| plugin.kind == kind && plugin.name.eq_ignore_ascii_case(name))
}

/// Look up a built-in plugin by its short name, whatever its family.
///
/// A bare name (`apikey`, `cors`) is not classified by [`PluginRef::parse`],
/// which only reads GTS ids and UUIDs; the catalog is what gives it a family.
#[must_use]
pub fn lookup_built_in_by_name(name: &str) -> Option<&'static BuiltInPlugin> {
    let wanted = name.trim();
    BUILT_IN_PLUGINS
        .iter()
        .find(|plugin| plugin.name.eq_ignore_ascii_case(wanted))
}

/// Whether the reference names a built-in the catalog does not let anyone bind
/// (`basic`, `bearer`, `timeout`, `cors`, `logging`, `metrics`).
///
/// Such a binding cannot exist at all, which is a different failure from an
/// implementation the *deployment* cannot honour.
#[must_use]
pub fn is_unbindable_built_in(reference: &str) -> bool {
    match PluginRef::parse(reference) {
        PluginRef::BuiltIn {
            resolvable: true, ..
        }
        | PluginRef::Custom { .. } => false,
        PluginRef::BuiltIn {
            resolvable: false, ..
        } => true,
        PluginRef::Unrecognised(_) => {
            lookup_built_in_by_name(reference).is_some_and(|plugin| !plugin.resolvable)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BUILT_IN_PLUGINS, PluginKind, PluginRef, lookup_built_in};

    #[test]
    fn every_kind_has_a_distinct_gts_stem() {
        let stems: Vec<&str> = PluginKind::all().iter().map(|k| k.gts_stem()).collect();
        assert_eq!(stems.len(), 3);
        assert!(stems.iter().all(|stem| stem.ends_with("_plugin.v1~")));
    }

    #[test]
    fn built_in_catalog_matches_the_design_tables() {
        let resolvable: Vec<&str> = BUILT_IN_PLUGINS
            .iter()
            .filter(|plugin| plugin.resolvable)
            .map(|plugin| plugin.name)
            .collect();
        assert_eq!(
            resolvable,
            vec![
                "noop",
                "apikey",
                "oauth2_client_cred",
                "oauth2_client_cred_basic",
                "required_headers",
                "request_id"
            ]
        );
        assert_eq!(BUILT_IN_PLUGINS.len(), 12);
    }

    #[test]
    fn built_in_references_are_classified() {
        let reference = PluginRef::parse("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1");
        assert!(reference.is_bindable_built_in());
        assert!(
            !PluginRef::parse("gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1")
                .is_bindable_built_in()
        );
    }

    #[test]
    fn custom_references_carry_the_uuid() {
        let id = uuid::Uuid::new_v4();
        let raw = format!("gts.cf.core.oagw.transform_plugin.v1~{id}");
        assert_eq!(PluginRef::parse(&raw).custom_id(), Some(id));
    }

    #[test]
    fn unknown_references_are_kept_verbatim() {
        assert_eq!(PluginRef::parse("not-a-plugin").raw(), "not-a-plugin");
    }

    #[test]
    fn bare_uuid_references_are_kind_agnostic_custom_ids() {
        let id = uuid::Uuid::new_v4();
        let parsed = PluginRef::parse(&id.to_string());
        assert_eq!(parsed.custom_id(), Some(id));
        assert_eq!(parsed.kind(), None);
        assert_eq!(parsed.raw(), id.to_string());
    }

    #[test]
    fn gts_custom_references_carry_their_kind() {
        let id = uuid::Uuid::new_v4();
        let parsed = PluginRef::parse(&format!("gts.cf.core.oagw.guard_plugin.v1~{id}"));
        assert_eq!(parsed.kind(), Some(PluginKind::Guard));
        assert_eq!(parsed.custom_id(), Some(id));
    }

    #[test]
    fn the_catalog_is_queried_by_kind_and_name() {
        assert_eq!(
            lookup_built_in(PluginKind::Auth, "noop").map(|plugin| plugin.name),
            Some("noop")
        );
        assert_eq!(
            lookup_built_in(PluginKind::Auth, "basic").map(|plugin| plugin.name),
            Some("basic")
        );
        assert!(lookup_built_in(PluginKind::Auth, "madeup").is_none());
        assert!(lookup_built_in(PluginKind::Guard, "noop").is_none());
    }
}
