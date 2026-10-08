//! Configuration of the static audit plugin.

use serde::Deserialize;

/// `gears.static-mini-chat-audit-plugin.config`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StaticAuditConfig {
    /// When `false` the plugin registers but does not log events.
    pub enabled: bool,
    pub vendor: String,
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

    use super::*;

    #[test]
    fn defaults_and_unknown_keys() {
        let cfg = StaticAuditConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.vendor, "constructorfabric");
        assert_eq!(cfg.priority, 100);

        let cfg: StaticAuditConfig = serde_json::from_value(json!({"enabled": false})).unwrap();
        assert!(!cfg.enabled);
        assert_eq!(cfg.priority, 100);

        assert!(serde_json::from_value::<StaticAuditConfig>(json!({"enable": true})).is_err());
    }
}
