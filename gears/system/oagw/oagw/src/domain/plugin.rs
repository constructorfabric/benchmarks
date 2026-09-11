//! `Plugin` aggregate (DESIGN §3.1, for which no schema is shipped).
//!
//! Custom tenant-defined Starlark plugins are persisted keyed by `id`; named
//! built-in plugins are resolved via an in-process registry and never stored.
//! The row carries the two members the create flow accepts beyond the class's
//! own — the optional description and the declared phases — because a plugin
//! created with them would otherwise be created with members no read could
//! return. Timestamps are unix seconds: no chrono dependency is available at
//! this layer, and no persistence type may appear here.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A custom tenant-defined plugin.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plugin {
    /// Plugin identifier; equals the UUID for a UUID-backed plugin.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// One of the three plugin-family literals: `auth`, `guard`, or
    /// `transform`. The literal selects the base type of the plugin's
    /// anonymous GTS identifier.
    pub plugin_type: String,
    /// Human-readable plugin name.
    pub name: String,
    /// Optional human-readable description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema of the plugin configuration, absent when the plugin
    /// declares none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<serde_json::Value>,
    /// The phases the plugin declares, as the wire literals.
    #[serde(default)]
    pub phases: Vec<String>,
    /// Starlark source of the plugin.
    pub source_code: String,
    /// Unix seconds of the last use, for garbage collection.
    #[serde(default)]
    pub last_used_at: Option<u64>,
    /// Unix seconds after which the plugin becomes eligible for collection.
    #[serde(default)]
    pub gc_eligible_at: Option<u64>,
}
