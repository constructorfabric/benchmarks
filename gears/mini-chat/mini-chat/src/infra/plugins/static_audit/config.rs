//! Configuration of the static audit plugin.

use serde::Deserialize;

/// Configuration error of the static audit plugin.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StaticAuditConfigError {
    /// The section could not be parsed (unknown key, wrong type).
    #[error("invalid static-mini-chat-audit-plugin configuration: {0}")]
    Parse(String),
}

/// Parsed `static-mini-chat-audit-plugin` section.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StaticAuditConfig {
    /// Whether audit events are logged.
    pub enabled: bool,
    /// Vendor of the registered GTS instance.
    pub vendor: String,
    /// Priority of the registered GTS instance; lower wins.
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

impl StaticAuditConfig {
    /// Parses the raw section; `None` (absent section) yields the defaults.
    ///
    /// # Errors
    ///
    /// Returns [`StaticAuditConfigError::Parse`] for unknown keys or wrong types.
    pub fn load(raw: Option<&serde_json::Value>) -> Result<Self, StaticAuditConfigError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-static-audit-plugin:p1:inst-parse-audit-config
        let Some(raw) = raw else {
            return Ok(Self::default());
        };
        serde_json::from_value(raw.clone())
            .map_err(|error| StaticAuditConfigError::Parse(error.to_string()))
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-static-audit-plugin:p1:inst-parse-audit-config
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
