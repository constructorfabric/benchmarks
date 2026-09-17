//! `Plugin` entity, plugin identification, and the plugin registry model.
//!
//! Mirrors `oagw_plugin` (§3.7): UUID-backed custom Starlark plugins persist
//! to `oagw_plugin`; named (built-in) plugins resolve via in-process
//! registries and are not persisted (base GTS type
//! `gts.cf.core.oagw.{type}_plugin.v1~*`).

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::time::SystemTime;
use uuid::Uuid;

/// Plugin category, mirroring `oagw_plugin.plugin_type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginType {
    Auth,
    Guard,
    Transform,
}

impl PluginType {
    /// The GTS type segment used in plugin identifiers and permissions
    /// (`auth_plugin` | `guard_plugin` | `transform_plugin`).
    #[must_use]
    pub const fn gts_type_segment(self) -> &'static str {
        match self {
            Self::Auth => "auth_plugin",
            Self::Guard => "guard_plugin",
            Self::Transform => "transform_plugin",
        }
    }
}

/// The `Plugin` entity — a UUID-backed custom plugin.
///
/// Immutable after creation; periodic GC deletes rows whose `gc_eligible_at`
/// is in the past (lifecycle owned by the Plugin System feature).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Plugin {
    /// Custom plugin identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    pub plugin_type: PluginType,
    /// Unique plugin name per tenant (`(tenant_id, name)` UNIQUE).
    pub name: String,
    /// JSON schema for plugin configuration.
    pub config_schema: JsonValue,
    /// Starlark source.
    pub source_code: String,
    /// Last binding usage.
    pub last_used_at: Option<SystemTime>,
    /// GC eligibility timestamp.
    pub gc_eligible_at: Option<SystemTime>,
    pub created_at: Option<SystemTime>,
    pub updated_at: Option<SystemTime>,
}

impl Default for Plugin {
    fn default() -> Self {
        Self {
            id: Uuid::new_v4(),
            tenant_id: Uuid::nil(),
            plugin_type: PluginType::Guard,
            name: String::new(),
            config_schema: JsonValue::Null,
            source_code: String::new(),
            last_used_at: None,
            gc_eligible_at: None,
            created_at: None,
            updated_at: None,
        }
    }
}

impl Plugin {
    /// Named constructor for a custom plugin.
    #[must_use]
    pub fn new(
        tenant_id: Uuid,
        plugin_type: PluginType,
        name: impl Into<String>,
        config_schema: JsonValue,
        source_code: impl Into<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            tenant_id,
            plugin_type,
            name: name.into(),
            config_schema,
            source_code: source_code.into(),
            ..Self::default()
        }
    }
}

/// Plugin configuration blob bound to a plugin reference.
///
/// Carried by [`PluginBinding`](super::config::PluginBinding) and validated
/// against the plugin's `config_schema` at the control plane.
// NOTE: no `Eq` — the wrapped `serde_json::Value` (float-capable) is not `Eq`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PluginConfig(pub JsonValue);

impl PluginConfig {
    /// The raw JSON configuration.
    #[must_use]
    pub fn as_json(&self) -> &JsonValue {
        &self.0
    }
}

/// Kind of resource referencing a plugin in the `referenced_by` shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferencedByResource {
    Upstream,
    Route,
}

impl ReferencedByResource {
    /// The wire token (`upstream` | `route`) of the referencing resource.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Upstream => "upstream",
            Self::Route => "route",
        }
    }
}

/// One upstream or route binding that references a custom plugin by UUID —
/// the `referenced_by` shape carried by the 409 `plugin.in_use` envelope
/// (management API, DoD
/// `cpt-cf-oagw-dod-control-plane-api-plugin-crud`; ADR
/// `cpt-cf-oagw-adr-request-routing`, "Plugin Deletion Behavior").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferencedBy {
    /// `upstream` or `route`.
    pub resource: ReferencedByResource,
    /// The referencing resource's UUID.
    pub id: Uuid,
}
