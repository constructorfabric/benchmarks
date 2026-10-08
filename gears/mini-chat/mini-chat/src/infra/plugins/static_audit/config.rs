//! Configuration of the static audit plugin
//! (`gears.static-mini-chat-audit-plugin.config`, DESIGN Appendix B.1 "Bundled plugins").

use serde::Deserialize;

/// Static audit plugin configuration.
#[derive(Debug, Clone, Deserialize, toolkit_macros::ExpandVars)]
#[serde(default, deny_unknown_fields)]
pub struct StaticAuditConfig {
    /// When `false` the plugin registers but does not log audit events.
    pub enabled: bool,
    /// Vendor of the registered GTS instance (must match the gear's `vendor`).
    pub vendor: String,
    /// Plugin priority (lower = higher priority).
    pub priority: i16,
}

impl Default for StaticAuditConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            vendor: "constructorfabric".to_owned(),
            priority: 100,
        }
    }
}
