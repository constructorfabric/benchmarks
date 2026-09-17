//! Configuration for the OAGW gear.
//!
//! The gear reads its settings from the `gears.oagw.config` section of the
//! runtime configuration:
//!
//! ```text
//! gears:
//!   oagw:
//!     config:
//!       proxy_timeout_secs: 2
//!       allow_http_upstream: true # Only for testing - do not enable in production without proper security controls
//!       ssrf_policy:
//!         enabled: false
//! ```
//!
//! Every key present in the shipped configuration is declared here, and
//! unknown keys are rejected (`deny_unknown_fields`) so a typo in the config
//! file fails at startup instead of being silently ignored.

use std::time::Duration;

use serde::Deserialize;

/// Default upstream proxy timeout in seconds, used when
/// `gears.oagw.config.proxy_timeout_secs` is absent.
pub const DEFAULT_PROXY_TIMEOUT_SECS: u64 = 30;

/// Server-Side Request Forgery protection policy
/// (`gears.oagw.config.ssrf_policy`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SsrfPolicy {
    /// Whether outbound proxy targets are validated against the SSRF policy
    /// before a connection is opened. Default: `false`.
    #[serde(default)]
    pub enabled: bool,
}

/// Configuration for the OAGW gear.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct OagwConfig {
    /// Timeout applied to proxied upstream requests, in seconds.
    /// Default: [`DEFAULT_PROXY_TIMEOUT_SECS`].
    pub proxy_timeout_secs: u64,

    /// Allow plain `http://` upstream targets. Disabled by default so the
    /// gateway only forwards to TLS-protected upstreams unless an operator
    /// opts in. Default: `false`.
    pub allow_http_upstream: bool,

    /// Server-Side Request Forgery protection policy. Default: disabled.
    pub ssrf_policy: SsrfPolicy,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: DEFAULT_PROXY_TIMEOUT_SECS,
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicy::default(),
        }
    }
}

impl OagwConfig {
    /// The proxy timeout as a [`Duration`].
    #[must_use]
    pub fn proxy_timeout(&self) -> Duration {
        Duration::from_secs(self.proxy_timeout_secs)
    }

    /// Whether SSRF protection is enabled for outbound requests.
    #[must_use]
    pub const fn ssrf_enabled(&self) -> bool {
        self.ssrf_policy.enabled
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    /// Shape of `gears.oagw.config` in `/app/config/e2e-local.yaml`
    /// (lines 461-466). YAML is a superset of JSON, so the document parses
    /// identically through `serde_json`.
    #[test]
    fn test_deserializes_e2e_local_config() {
        let json = serde_json::json!({
            "proxy_timeout_secs": 2,
            "allow_http_upstream": true, // Only for testing
            "ssrf_policy": {
                "enabled": false,
            }
        });

        let cfg: OagwConfig = serde_json::from_value(json).unwrap();

        assert_eq!(cfg.proxy_timeout_secs, 2);
        assert!(cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.enabled);
        assert!(!cfg.ssrf_enabled());
        assert_eq!(cfg.proxy_timeout(), Duration::from_secs(2));
    }

    #[test]
    fn test_empty_map_uses_documented_defaults() {
        let cfg: OagwConfig = serde_json::from_value(serde_json::json!({})).unwrap();

        assert_eq!(cfg.proxy_timeout_secs, DEFAULT_PROXY_TIMEOUT_SECS);
        assert_eq!(cfg.proxy_timeout_secs, 30);
        assert!(!cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.enabled);
        assert_eq!(cfg.proxy_timeout(), Duration::from_secs(30));
    }

    #[test]
    fn test_default_impl_matches_documented_defaults() {
        let cfg = OagwConfig::default();

        assert_eq!(cfg.proxy_timeout_secs, 30);
        assert!(!cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.enabled);
    }

    #[test]
    fn test_partial_map_fills_missing_keys_from_defaults() {
        let cfg: OagwConfig =
            serde_json::from_value(serde_json::json!({ "proxy_timeout_secs": 5 })).unwrap();

        assert_eq!(cfg.proxy_timeout_secs, 5);
        assert!(!cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.enabled);
    }

    #[test]
    fn test_ssrf_policy_defaults_to_disabled() {
        let policy: SsrfPolicy = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(!policy.enabled);

        let policy: SsrfPolicy =
            serde_json::from_value(serde_json::json!({ "enabled": true })).unwrap();
        assert!(policy.enabled);
    }

    #[test]
    fn test_unknown_keys_are_rejected() {
        let result: Result<OagwConfig, _> =
            serde_json::from_value(serde_json::json!({ "not_a_key": true }));

        assert!(result.is_err(), "deny_unknown_fields must reject typos");
    }
}
