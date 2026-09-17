//! Gear configuration (`gears.oagw.config`).
//!
//! The defaults line up with `config/e2e-local.yaml`'s `oagw.config`
//! section, and every field is optional so the gear also starts with no
//! configuration section at all (lenient loading).
use serde::{Deserialize, Serialize};

use crate::domain::services::ListLimits;

/// Default upstream exchange timeout, in seconds.
const DEFAULT_PROXY_TIMEOUT_SECS: u64 = 2;

/// Configuration of the `oagw` gear.
///
/// Only the management plane consumes any of these values in this revision;
/// the proxy data plane (second dispatch) reads
/// [`OagwConfig::proxy_timeout_secs`], [`OagwConfig::allow_http_upstream`]
/// and [`OagwConfig::ssrf_policy`].
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct OagwConfig {
    /// Upstream exchange timeout, in seconds.
    pub proxy_timeout_secs: u64,
    /// Whether a plaintext `http` endpoint scheme may actually be dialled.
    ///
    /// `http` is always a *legal* `server.endpoints[].scheme` value in the
    /// management API; this flag only governs whether the proxy is allowed to
    /// produce the resulting cleartext connection.
    pub allow_http_upstream: bool,
    /// Outbound request screening (SSRF) policy.
    pub ssrf_policy: SsrfPolicy,
    /// Management-plane list pagination bounds.
    pub list: ListSettings,
    /// Plugin execution settings.
    pub plugins: PluginSettings,
}

/// Outbound request screening (SSRF) policy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct SsrfPolicy {
    /// Reject upstream targets that resolve into link-local, loopback or
    /// private address space.
    pub enabled: bool,
}

/// Management-plane list pagination bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ListSettings {
    /// `$top` applied when a list request omits it.
    pub default_top: usize,
    /// Largest accepted `$top`.
    pub max_top: usize,
}

/// Plugin execution settings.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct PluginSettings {
    /// Upper bound on the Starlark interpreter's memory budget per request.
    pub max_source_bytes: usize,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: DEFAULT_PROXY_TIMEOUT_SECS,
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicy::default(),
            list: ListSettings::default(),
            plugins: PluginSettings::default(),
        }
    }
}

impl Default for ListSettings {
    fn default() -> Self {
        let limits = ListLimits::default();
        Self {
            default_top: limits.default_top,
            max_top: limits.max_top,
        }
    }
}

impl Default for PluginSettings {
    fn default() -> Self {
        Self {
            max_source_bytes: 64 * 1024,
        }
    }
}

impl From<&OagwConfig> for ListLimits {
    fn from(config: &OagwConfig) -> Self {
        Self {
            default_top: config.list.default_top,
            max_top: config.list.max_top,
        }
    }
}

impl OagwConfig {
    /// Validate the configuration.
    ///
    /// # Errors
    ///
    /// A static string describing the first violation found.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.list.default_top == 0 {
            return Err("list.default_top must be greater than zero");
        }
        if self.list.max_top < self.list.default_top {
            return Err("list.max_top must not be smaller than list.default_top");
        }
        if self.proxy_timeout_secs == 0 {
            return Err("proxy_timeout_secs must be greater than zero");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_contract() {
        let config = OagwConfig::default();
        assert_eq!(config.proxy_timeout_secs, 2);
        assert!(!config.allow_http_upstream);
        assert!(!config.ssrf_policy.enabled);
        assert_eq!(config.list.default_top, 50);
        assert_eq!(config.list.max_top, 100);
        assert_eq!(config.validate(), Ok(()));
    }

    #[test]
    fn deserializes_the_runtime_section() {
        // Mirrors config/e2e-local.yaml `gears.oagw.config` verbatim.
        let json = serde_json::json!({
            "proxy_timeout_secs": 2,
            "allow_http_upstream": true,
            "ssrf_policy": { "enabled": false },
        });
        let config: OagwConfig = serde_json::from_value(json).expect("runtime section parses");
        assert!(config.allow_http_upstream);
        assert!(!config.ssrf_policy.enabled);
        assert_eq!(config.proxy_timeout_secs, 2);
        // Untouched sections fall back to their defaults.
        assert_eq!(config.list, ListSettings::default());
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let result = serde_json::from_value::<OagwConfig>(serde_json::json!({ "nope": 1 }));
        assert!(result.is_err(), "unknown fields must be rejected");
    }

    #[test]
    fn rejects_a_default_top_above_the_maximum() {
        let config = OagwConfig {
            list: ListSettings {
                default_top: 200,
                max_top: 100,
            },
            ..OagwConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn list_limits_follow_the_config() {
        let config = OagwConfig {
            list: ListSettings {
                default_top: 10,
                max_top: 20,
            },
            ..OagwConfig::default()
        };
        let limits = ListLimits::from(&config);
        assert_eq!(limits.default_top, 10);
        assert_eq!(limits.max_top, 20);
    }
}
