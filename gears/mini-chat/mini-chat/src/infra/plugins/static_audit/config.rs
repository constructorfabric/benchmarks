//! Configuration of the static audit plugin (DESIGN B.1).

use serde::Deserialize;

/// Plugin configuration: `gears.static-mini-chat-audit-plugin.config`. Unknown keys are rejected.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StaticAuditConfig {
    /// When `false`, the plugin registers but does not log audit events.
    pub enabled: bool,
    /// Vendor of the registered GTS instance.
    pub vendor: String,
    /// Lower is higher priority.
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::StaticAuditConfig;

    #[test]
    fn defaults_enable_the_plugin() {
        let cfg = StaticAuditConfig::default();

        assert!(cfg.enabled);
        assert_eq!(cfg.vendor, "constructorfabric");
        assert_eq!(cfg.priority, 100);
    }

    #[test]
    fn partial_sections_fill_the_defaults_and_unknown_keys_are_rejected() {
        let cfg: StaticAuditConfig = serde_json::from_value(json!({ "enabled": false })).unwrap();
        assert!(!cfg.enabled);
        assert_eq!(cfg.priority, 100);

        assert!(serde_json::from_value::<StaticAuditConfig>(json!({ "bogus": 1 })).is_err());
    }
}
