//! Plugin domain model (`gts.cf.core.oagw.{auth,guard,transform}_plugin.v1~`).
//!
//! Two plugin populations exist (DESIGN.md "Plugin Identification Model"):
//!
//! * **Named** plugins — built-in or contributed by other gears — are
//!   identified by their full GTS id (`gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1`)
//!   and resolved through an in-process registry. They are *not* stored in the
//!   plugin resource table and are not subject to garbage collection.
//! * **Custom** plugins — tenant-defined, UUID-backed — are managed by the
//!   `/oagw/v1/plugins` CRUD API and stored in the plugin table keyed by their
//!   bare UUID.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::SharingMode;

/// Which of the three plugin kinds a resource represents.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginKind {
    /// Credential injection (`auth_plugin`).
    #[default]
    Auth,
    /// Validation / policy enforcement (`guard_plugin`).
    Guard,
    /// Request/response mutation (`transform_plugin`).
    Transform,
}

impl PluginKind {
    /// Wire representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Guard => "guard",
            Self::Transform => "transform",
        }
    }

    /// The GTS *type id* of this plugin kind.
    #[must_use]
    pub fn type_id(self) -> &'static str {
        match self {
            Self::Auth => crate::domain::gts_helpers::OAGW_AUTH_PLUGIN_TYPE_ID,
            Self::Guard => crate::domain::gts_helpers::OAGW_GUARD_PLUGIN_TYPE_ID,
            Self::Transform => crate::domain::gts_helpers::OAGW_TRANSFORM_PLUGIN_TYPE_ID,
        }
    }

    /// Derive the kind from a plugin type id (or any plugin identifier).
    #[must_use]
    pub fn from_type_id(value: &str) -> Option<Self> {
        if value.starts_with(crate::domain::gts_helpers::OAGW_AUTH_PLUGIN_TYPE_ID) {
            Some(Self::Auth)
        } else if value.starts_with(crate::domain::gts_helpers::OAGW_GUARD_PLUGIN_TYPE_ID) {
            Some(Self::Guard)
        } else if value.starts_with(crate::domain::gts_helpers::OAGW_TRANSFORM_PLUGIN_TYPE_ID) {
            Some(Self::Transform)
        } else {
            None
        }
    }
}

impl std::fmt::Display for PluginKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where the executable content of a custom plugin comes from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginSource {
    /// Inline Starlark (or equivalent) source held with the plugin record.
    #[default]
    Inline,
    /// Content referenced from an external location (not fetched by this gear).
    Reference,
}

/// A tenant-defined (UUID-backed) plugin resource.
///
/// Plugin definitions are immutable after creation (PRD "plugin system"):
/// updates are performed by creating a new plugin version and re-binding
/// references, so the management API offers `POST`/`GET`/`DELETE` but no `PUT`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Plugin {
    /// Server-generated identifier (UUID v4); the instance suffix of the
    /// plugin's GTS instance id.
    pub id: Uuid,
    /// Owning tenant (internal; never serialised to the management API).
    #[serde(skip)]
    pub tenant_id: Uuid,
    /// Whether the plugin may be bound to new upstreams/routes.
    pub enabled: bool,
    /// Operator-facing name.
    pub name: String,
    /// Operator-facing description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Which of the three plugin kinds this is.
    pub plugin_type: PluginKind,
    /// Implementation identifier of the backing executable
    /// (e.g. `starlark`), when the plugin has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub implementation: Option<String>,
    /// How the plugin participates in tenant-hierarchy merging.
    pub sharing: SharingMode,
    /// Flat tags for categorisation and discovery.
    pub tags: Vec<String>,
    /// Default configuration; merged with per-binding `config`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
    /// Declared configuration schema (`config_schema`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<serde_json::Value>,
    /// Phases the plugin participates in (`on_request`, `on_response`, `on_error`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub phases: Vec<String>,
    /// Declared source / provenance of the plugin content.
    #[serde(default)]
    pub source: PluginSourceRecord,
}

/// Declared source content of a plugin (returned by `GET /plugins/{id}/source`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginSourceRecord {
    /// How the content is delivered.
    pub kind: PluginSource,
    /// Inline source text (Starlark), when `kind` is
    /// [`PluginSource::Inline`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_code: Option<String>,
    /// Language of the source, when inline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// External location, when `kind` is [`PluginSource::Reference`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
}

impl Default for Plugin {
    fn default() -> Self {
        Self {
            id: Uuid::new_v4(),
            tenant_id: Uuid::nil(),
            enabled: true,
            name: String::new(),
            description: None,
            plugin_type: PluginKind::Transform,
            implementation: None,
            sharing: SharingMode::default(),
            tags: Vec::new(),
            config: None,
            config_schema: None,
            phases: Vec::new(),
            source: PluginSourceRecord::default(),
        }
    }
}

impl Plugin {
    /// GTS instance id of this plugin.
    #[must_use]
    pub fn gts_id(&self) -> String {
        crate::domain::gts_helpers::gts_instance_id(self.plugin_type.type_id(), &self.id)
    }
}
