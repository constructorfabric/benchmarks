//! The `Plugin` entity and the binding model.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{ErrorKind, OagwError};

/// Identifier prefix of the plugin GTS type.
pub use crate::types::PLUGIN_TYPE;

/// Builds a full GTS identifier for a plugin.
#[must_use]
pub fn plugin_id(id: Uuid) -> String {
    format!("{PLUGIN_TYPE}~{id}")
}

/// What a plugin does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PluginKind {
    /// Injects credentials into the outbound request.
    Auth,
    /// Validates the request or the response and may reject it.
    Guard,
    /// Rewrites headers or metadata on either side.
    Transform,
}

impl PluginKind {
    /// The identifier as it appears in a plugin definition.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Guard => "guard",
            Self::Transform => "transform",
        }
    }
}

/// Built-in auth plugin identifiers that can be bound.
pub const AUTH_PLUGINS: [&str; 4] = [
    "noop",
    "apikey",
    "oauth2_client_cred",
    "oauth2_client_cred_basic",
];

/// Auth plugins present in the type catalog that fail when selected.
pub const CATALOG_ONLY_AUTH_PLUGINS: [&str; 2] = ["basic", "bearer"];

/// The only guard identifier that may be bound to a plugin list.
pub const BINDABLE_GUARD_PLUGINS: [&str; 1] = ["required_headers"];

/// Guard identifiers that exist for cataloguing only.
pub const CATALOG_ONLY_GUARD_PLUGINS: [&str; 2] = ["timeout", "cors"];

/// Transform identifiers that may be bound.
pub const BINDABLE_TRANSFORM_PLUGINS: [&str; 1] = ["request_id"];

/// Guard/transform identifiers that exist only in the catalog.
pub const CATALOG_ONLY_TRANSFORM_PLUGINS: [&str; 2] = ["logging", "metrics"];

/// Where a plugin binding applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Bound to an upstream.
    Upstream,
    /// Bound to a route.
    Route,
}

/// An ordered association of a plugin with an upstream or route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PluginBinding {
    /// The plugin's name: a built-in identifier or a resolvable plugin id.
    pub name: String,
    /// Optional UUID of a persisted plugin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    /// Per-binding configuration.
    #[serde(default)]
    pub config: serde_json::Map<String, serde_json::Value>,
}

/// The `Plugin` entity.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plugin {
    /// Server-generated GTS identifier.
    #[serde(default)]
    pub id: String,
    /// Owning tenant.
    #[serde(default)]
    pub tenant_id: Uuid,
    /// Human-readable name.
    #[serde(default)]
    pub name: String,
    /// What the plugin does. The wire name is `plugin_type`, which the component
    /// contract spells out; the field avoids the type's own prefix.
    #[serde(default, rename = "plugin_type")]
    pub kind: Option<PluginKind>,
    /// Optional configuration schema.
    #[serde(default)]
    pub config_schema: Option<serde_json::Value>,
    /// Source content served by `GET /plugins/{id}/source`.
    #[serde(default)]
    pub source: String,
}

impl Default for Plugin {
    fn default() -> Self {
        Self {
            id: String::new(),
            tenant_id: Uuid::nil(),
            name: String::new(),
            kind: None,
            config_schema: None,
            source: String::new(),
        }
    }
}

/// Whether an identifier is a known built-in plugin name.
#[must_use]
pub fn is_builtin(name: &str) -> bool {
    AUTH_PLUGINS.contains(&name)
        || BINDABLE_GUARD_PLUGINS.contains(&name)
        || BINDABLE_TRANSFORM_PLUGINS.contains(&name)
        || CATALOG_ONLY_AUTH_PLUGINS.contains(&name)
        || CATALOG_ONLY_GUARD_PLUGINS.contains(&name)
        || CATALOG_ONLY_TRANSFORM_PLUGINS.contains(&name)
}

/// The `PluginKind` a built-in identifier belongs to.
#[must_use]
pub fn builtin_kind(name: &str) -> Option<PluginKind> {
    if AUTH_PLUGINS.contains(&name) || CATALOG_ONLY_AUTH_PLUGINS.contains(&name) {
        Some(PluginKind::Auth)
    } else if BINDABLE_GUARD_PLUGINS.contains(&name) || CATALOG_ONLY_GUARD_PLUGINS.contains(&name) {
        Some(PluginKind::Guard)
    } else if BINDABLE_TRANSFORM_PLUGINS.contains(&name)
        || CATALOG_ONLY_TRANSFORM_PLUGINS.contains(&name)
    {
        Some(PluginKind::Transform)
    } else {
        None
    }
}

/// Validate a list of plugin bindings.
///
/// # Errors
///
/// Returns a validation error when a binding names an unknown plugin, names a guard
/// other than `required_headers`, or references a UUID that is not supplied.
pub fn validate_bindings(bindings: &[PluginBinding], _stage: Stage) -> Result<(), OagwError> {
    for binding in bindings {
        let kind = builtin_kind(&binding.name);
        match kind {
            Some(PluginKind::Guard) => {
                if !BINDABLE_GUARD_PLUGINS.contains(&binding.name.as_str()) {
                    return Err(OagwError::new(
                        ErrorKind::ValidationError,
                        format!("guard plugin `{}` cannot be bound", binding.name),
                    ));
                }
            }
            Some(_) => {}
            None => {
                // Not a built-in: must be a UUID reference that resolves.
                if Uuid::parse_str(&binding.name).is_err() && binding.uuid.is_none() {
                    return Err(OagwError::new(
                        ErrorKind::ValidationError,
                        format!("plugin `{}` is not a known identifier", binding.name),
                    ));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "plugin_tests.rs"]
mod tests;
