use serde::Deserialize;

/// Plugin configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StaticMiniChatAuditPluginConfig {
    /// When `false`, the plugin registers but does not emit audit events.
    /// Defaults to `true`.
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    /// Vendor name for GTS instance registration.
    #[serde(default = "default_vendor")]
    pub vendor: String,

    /// Plugin priority (lower = higher priority).
    #[serde(default = "default_priority")]
    pub priority: i16,
}

impl Default for StaticMiniChatAuditPluginConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            vendor: default_vendor(),
            priority: default_priority(),
        }
    }
}

const fn default_enabled() -> bool {
    true
}

fn default_vendor() -> String {
    "constructorfabric".to_owned()
}

const fn default_priority() -> i16 {
    100
}
