//! Plugin domain/DTO types.
//!
//! Plugin has no dedicated JSON schema file (unlike Upstream/Route); this
//! type follows `DESIGN.md` §3.1's domain-model class diagram for the
//! `Plugin` entity instead: custom, tenant-defined Starlark plugins stored
//! in `oagw_plugin` (`gts.cf.core.oagw.{type}_plugin.v1~`). Named (built-in)
//! plugins are resolved via an in-process registry and are never persisted
//! as a `Plugin` record.
//!
//! `cpt-cf-oagw-feature-plugin-management` (2.4) owns this file: the
//! `Plugin` storage shape, immutability, the GTS plugin-identification model
//! ([`identity`]), and the reference-counting / GC-bookkeeping algorithms
//! ([`lifecycle`]) that `cpt-cf-oagw-feature-upstream-management` (2.2),
//! `cpt-cf-oagw-feature-route-management` (2.3) and
//! `cpt-cf-oagw-feature-plugin-execution` (2.9) reuse.

pub mod identity;
pub mod lifecycle;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

pub use identity::{
    PluginIdentifier, PluginIdentifierError, PluginLookupError, PluginResolutionError,
    ResolvedPluginRef, lookup_plugin_for_management, named_plugin_gts_ref, parse_plugin_identifier,
    plugin_gts_ref, resolve_plugin_ref,
};
pub use lifecycle::{PluginReferences, count_plugin_references, mark_gc_eligibility};

/// Which of the three plugin traits (`DESIGN.md` §3.4) a custom Plugin
/// implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PluginType {
    /// Credential injection. One per upstream.
    Auth,
    /// Validation/policy enforcement (can reject). Multiple per
    /// upstream/route.
    Guard,
    /// Request/response mutation. Multiple per upstream/route.
    Transform,
}

/// One entry in a `PluginType`'s named-plugin catalog
/// (`cpt-cf-oagw-dod-plugin-identification`).
///
/// `has_backing_implementation` records the ADR-0002 "Built-in Plugins"
/// distinction this feature must not blur: some catalog tokens (e.g.
/// `basic`/`bearer` for Auth, `timeout`/`cors` for Guard, `logging`/`metrics`
/// for Transform) are reserved GTS identifiers cataloged in the
/// types-registry with **no backing trait implementation** in
/// `infra/plugin/` -- they classify as `kind: "named"` and resolve
/// successfully via [`identity::resolve_plugin_ref`] (identification is this
/// feature's whole job), but a caller that needs to know whether the token
/// can actually be *executed* (`cpt-cf-oagw-feature-plugin-execution`, 2.9)
/// must consult this flag rather than assume every named token is backed.
/// ADR-0002's illustrative "Plugin Loading" code snippet registers a
/// `BasicAuthPlugin` that contradicts its own normative prose (which states
/// `basic`/`bearer` have no backing implementation); this table follows the
/// prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NamedPluginCatalogEntry {
    /// The token portion of the named identifier, e.g. `"apikey"`.
    pub token: &'static str,
    /// Whether `infra/plugin/` carries a real trait implementation for this
    /// token (ADR-0002 "Built-in Plugins"), as opposed to a types-registry
    /// cataloging-only reservation.
    pub has_backing_implementation: bool,
}

const fn entry(token: &'static str, has_backing_implementation: bool) -> NamedPluginCatalogEntry {
    NamedPluginCatalogEntry {
        token,
        has_backing_implementation,
    }
}

const AUTH_NAMED_CATALOG: &[NamedPluginCatalogEntry] = &[
    entry("noop", true),
    entry("apikey", true),
    entry("oauth2_client_cred", true),
    entry("oauth2_client_cred_basic", true),
    entry("basic", false),
    entry("bearer", false),
];

const GUARD_NAMED_CATALOG: &[NamedPluginCatalogEntry] = &[
    entry("required_headers", true),
    entry("timeout", false),
    entry("cors", false),
];

const TRANSFORM_NAMED_CATALOG: &[NamedPluginCatalogEntry] = &[
    entry("request_id", true),
    entry("logging", false),
    entry("metrics", false),
];

impl PluginType {
    /// The `{type}` URL/GTS segment for this plugin type (`"auth"` /
    /// `"guard"` / `"transform"`), used both in the `{type}_plugin` GTS
    /// segment and in the per-type management permission names.
    #[must_use]
    pub const fn url_segment(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Guard => "guard",
            Self::Transform => "transform",
        }
    }

    /// Parse a `{type}` segment (as it appears before `_plugin` in
    /// `gts.cf.core.oagw.{type}_plugin.v1~...`) back into a [`PluginType`].
    #[must_use]
    pub fn from_url_segment(segment: &str) -> Option<Self> {
        match segment {
            "auth" => Some(Self::Auth),
            "guard" => Some(Self::Guard),
            "transform" => Some(Self::Transform),
            _ => None,
        }
    }

    /// This type's named-plugin catalog (`cpt-cf-oagw-dod-plugin-identification`):
    /// every token classifiable as `kind: "named"` for this `plugin_type`,
    /// each with its ADR-0002 backing-implementation flag.
    #[must_use]
    pub const fn named_catalog(self) -> &'static [NamedPluginCatalogEntry] {
        match self {
            Self::Auth => AUTH_NAMED_CATALOG,
            Self::Guard => GUARD_NAMED_CATALOG,
            Self::Transform => TRANSFORM_NAMED_CATALOG,
        }
    }

    /// Look up one named-catalog entry for this type by its bare token
    /// (e.g. `"apikey"`), or `None` if `token` is not a member of this
    /// type's catalog.
    #[must_use]
    pub fn named_catalog_entry(self, token: &str) -> Option<NamedPluginCatalogEntry> {
        self.named_catalog()
            .iter()
            .find(|candidate| candidate.token == token)
            .copied()
    }
}

/// Custom tenant-defined Starlark plugin record. Immutable after creation
/// (`DESIGN.md` §4.9 "Plugin immutability") -- a change is represented as a
/// new plugin plus a rebind, never an in-place update
/// (`cpt-cf-oagw-dod-plugin-immutability`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Plugin {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<Uuid>,
    pub plugin_type: PluginType,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<serde_json::Value>,
    pub source_code: String,
    /// RFC 3339 timestamp string, written only by the (out-of-scope)
    /// `cpt-cf-oagw-feature-plugin-execution` (2.9) data path; this feature
    /// initializes it to `NULL` at creation and never updates it thereafter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<String>,
    /// RFC 3339 timestamp string; mark-phase bookkeeping owned by
    /// [`lifecycle::mark_gc_eligibility`] (`cpt-cf-oagw-algo-plugin-gc-mark-sweep`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gc_eligible_at: Option<String>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_a_minimal_guard_plugin() {
        let json = serde_json::json!({
            "plugin_type": "guard",
            "name": "required-headers",
            "source_code": "def guard(req): return req",
        });
        let plugin: Plugin = serde_json::from_value(json).unwrap();
        assert_eq!(plugin.plugin_type, PluginType::Guard);
        assert!(plugin.id.is_none());
    }

    #[test]
    fn url_segment_and_from_url_segment_round_trip_for_every_type() {
        for plugin_type in [PluginType::Auth, PluginType::Guard, PluginType::Transform] {
            let segment = plugin_type.url_segment();
            assert_eq!(PluginType::from_url_segment(segment), Some(plugin_type));
        }
        assert_eq!(PluginType::from_url_segment("bogus"), None);
    }

    #[test]
    fn named_catalog_distinguishes_backed_from_catalog_only_tokens() {
        let apikey = PluginType::Auth.named_catalog_entry("apikey").unwrap();
        assert!(apikey.has_backing_implementation);

        let basic = PluginType::Auth.named_catalog_entry("basic").unwrap();
        assert!(!basic.has_backing_implementation);

        let timeout = PluginType::Guard.named_catalog_entry("timeout").unwrap();
        assert!(!timeout.has_backing_implementation);

        let required_headers = PluginType::Guard
            .named_catalog_entry("required_headers")
            .unwrap();
        assert!(required_headers.has_backing_implementation);

        assert_eq!(
            PluginType::Auth.named_catalog_entry("required_headers"),
            None
        );
    }

    #[test]
    fn named_catalog_covers_exactly_the_twelve_documented_tokens() {
        let all: Vec<&str> = [
            PluginType::Auth.named_catalog(),
            PluginType::Guard.named_catalog(),
            PluginType::Transform.named_catalog(),
        ]
        .into_iter()
        .flat_map(|catalog| catalog.iter().map(|entry| entry.token))
        .collect();
        assert_eq!(all.len(), 12);
    }
}
