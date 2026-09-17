//! Logging identifier — **catalog identifier only**
//! (`gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1`).
//!
//! Request/response logging is *core Data Plane instrumentation* (`tracing`
//! and `infra/metrics.rs`), not a `TransformPlugin` trait implementation. The
//! identifier exists for types-registry cataloging only and is deliberately
//! **not** resolvable via
//! [`TransformPluginRegistry`](crate::infra::plugin::TransformPluginRegistry).

/// GTS instance id of the catalog-only logging transform.
pub use crate::domain::gts_helpers::LOGGING_TRANSFORM_PLUGIN_ID;

/// Audit-log event names (ADR 0001 "Audit Log JSON Format").
pub mod events {
    /// A proxied request completed.
    pub const REQUEST_COMPLETED: &str = "oagw.request.completed";
    /// A proxied request failed.
    pub const REQUEST_FAILED: &str = "oagw.request.failed";
    /// A control-plane resource changed.
    pub const RESOURCE_CHANGED: &str = "oagw.resource.changed";
}

/// Structured logging configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoggingConfig {
    /// Minimum level that emits an event (`info` unless overridden).
    pub level: tracing::Level,
    /// Whether request/response bodies are logged.
    pub log_bodies: bool,
    /// Headers whose values are never emitted.
    pub redacted_headers: Vec<String>,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: tracing::Level::INFO,
            log_bodies: false,
            redacted_headers: vec![
                "authorization".to_owned(),
                "proxy-authorization".to_owned(),
                "x-api-key".to_owned(),
                "cookie".to_owned(),
                "set-cookie".to_owned(),
            ],
        }
    }
}

impl LoggingConfig {
    /// Parse a configuration from a raw plugin/config value.
    #[must_use]
    pub fn from_config(config: &serde_json::Value) -> Self {
        let mut cfg = Self::default();
        if let Some(bodies) = config.get("log_bodies").and_then(|v| v.as_bool()) {
            cfg.log_bodies = bodies;
        }
        if let Some(headers) = config.get("redacted_headers").and_then(|v| v.as_array()) {
            let names: Vec<String> = headers
                .iter()
                .filter_map(|v| v.as_str())
                .map(|v| v.to_ascii_lowercase())
                .collect();
            if !names.is_empty() {
                cfg.redacted_headers = names;
            }
        }
        cfg
    }

    /// True when a header value must be elided from the log line.
    #[must_use]
    pub fn is_redacted(&self, header: &str) -> bool {
        self.redacted_headers
            .iter()
            .any(|h| h.eq_ignore_ascii_case(header))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::plugin::registry::TransformPluginRegistry;

    #[test]
    fn identifier_is_catalog_only() {
        assert!(
            TransformPluginRegistry::with_builtins()
                .get(LOGGING_TRANSFORM_PLUGIN_ID)
                .is_none()
        );
    }

    #[test]
    fn secrets_are_redacted() {
        let cfg = LoggingConfig::default();
        assert!(cfg.is_redacted("Authorization"));
        assert!(!cfg.is_redacted("content-type"));
    }
}
