//! Config of `static-mini-chat-audit-plugin` (DESIGN Appendix B.1).

use serde::Deserialize;

/// `gears.static-mini-chat-audit-plugin.config`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StaticAuditPluginConfig {
    pub vendor: String,
    pub priority: i16,
    /// When `false` the plugin registers but does not log audit events.
    pub enabled: bool,
}

impl Default for StaticAuditPluginConfig {
    fn default() -> Self {
        Self {
            vendor: "constructorfabric".to_owned(),
            priority: 100,
            enabled: true,
        }
    }
}
