//! Timeout guard identifier — **catalog identifier only**
//! (`gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1`).
//!
//! Request timeout is *core Data Plane configuration*, not a `GuardPlugin`
//! trait implementation (ADR 0002, ADR 0009): the identifier exists so the
//! types-registry can catalog the concept, but it is deliberately **not**
//! resolvable via
//! [`GuardPluginRegistry`](crate::infra::plugin::GuardPluginRegistry).
//!
//! The data plane reads [`TimeoutConfig`] from the resolved upstream/route
//! configuration and applies it to the outbound exchange.

/// GTS instance id of the catalog-only timeout guard.
pub use crate::domain::gts_helpers::TIMEOUT_GUARD_PLUGIN_ID;

/// Timeout configuration for a proxied request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeoutConfig {
    /// Time to establish the upstream connection.
    pub connect_timeout_secs: u64,
    /// Time for the full upstream response (headers + body).
    pub request_timeout_secs: u64,
    /// Maximum idle period inside a streaming exchange.
    pub idle_timeout_secs: u64,
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            connect_timeout_secs: 5,
            request_timeout_secs: 30,
            idle_timeout_secs: 300,
        }
    }
}

impl TimeoutConfig {
    /// Parse a configuration from a raw plugin/config value.
    #[must_use]
    pub fn from_config(config: &serde_json::Value) -> Self {
        let u = |k: &str| config.get(k).and_then(|v| v.as_u64());
        Self {
            connect_timeout_secs: u("connect_timeout_secs").unwrap_or(5),
            request_timeout_secs: u("request_timeout_secs").unwrap_or(30),
            idle_timeout_secs: u("idle_timeout_secs").unwrap_or(300),
        }
    }

    /// Effective request timeout, clamped by the gear-wide budget.
    #[must_use]
    pub fn effective_request_timeout(&self, proxy_timeout_secs: u64) -> std::time::Duration {
        std::time::Duration::from_secs(self.request_timeout_secs.min(proxy_timeout_secs.max(1)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::plugin::registry::GuardPluginRegistry;

    #[test]
    fn identifier_is_catalog_only() {
        assert!(
            GuardPluginRegistry::with_builtins()
                .get(TIMEOUT_GUARD_PLUGIN_ID)
                .is_none()
        );
    }

    #[test]
    fn clamps_to_the_proxy_budget() {
        let cfg = TimeoutConfig::from_config(&serde_json::json!({"request_timeout_secs": 900}));
        assert_eq!(
            cfg.effective_request_timeout(2),
            std::time::Duration::from_secs(2)
        );
    }
}
