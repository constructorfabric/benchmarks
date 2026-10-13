//! Configuration of the static audit plugin (DESIGN B.1 "Bundled plugins").

use serde::Deserialize;

/// `gears.static-mini-chat-audit-plugin.config`; unknown keys are errors, an
/// absent section uses the defaults.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StaticAuditConfig {
    /// When `false` the plugin registers but does not log events.
    pub enabled: bool,
    /// Vendor of the registered GTS instance (must match the gear's `vendor`).
    pub vendor: String,
    /// Plugin priority (lower wins).
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
