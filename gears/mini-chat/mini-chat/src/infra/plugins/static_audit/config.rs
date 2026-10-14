//! Plugin configuration (DESIGN Appendix B.1, `static-mini-chat-audit-plugin`).

use serde::Deserialize;

use crate::config::ConfigError;

/// Default vendor of the registered plugin instance.
const DEFAULT_VENDOR: &str = "constructorfabric";
/// Default priority of the registered plugin instance (lower is higher priority).
const DEFAULT_PRIORITY: i16 = 100;

/// The plugin config section: `enabled` (default `true`), `vendor` (default
/// `constructorfabric`) and `priority` (default `100`, lower is higher priority). Unknown
/// top-level keys are rejected.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StaticAuditConfig {
    pub enabled: bool,
    pub vendor: String,
    pub priority: i16,
}

impl Default for StaticAuditConfig {
    /// The defaults of an absent section.
    fn default() -> Self {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-static-audit-plugin:p1:inst-static-audit-defaults
        Self {
            enabled: true,
            vendor: DEFAULT_VENDOR.to_owned(),
            priority: DEFAULT_PRIORITY,
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-static-audit-plugin:p1:inst-static-audit-defaults
    }
}

impl StaticAuditConfig {
    /// Parse a present config section; missing keys take their defaults.
    ///
    /// # Errors
    ///
    /// A [`ConfigError`] naming the offending key.
    pub fn parse(raw: serde_json::Value) -> Result<Self, ConfigError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-static-audit-plugin:p1:inst-static-audit-load
        // an unknown top-level key is a parse error that names it
        serde_json::from_value(raw).map_err(|e| ConfigError::Parse(format!("{e}")))
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-static-audit-plugin:p1:inst-static-audit-load
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod config_tests;
