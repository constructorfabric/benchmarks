// Created: 2026-09-03 by Constructor Tech
//! Gear configuration for OAGW, read from `gears.oagw.config`.

use std::time::Duration;

use serde::Deserialize;

/// Request body hard limit: 100 MB (`cpt-cf-oagw-constraint-body-limit`).
pub const DEFAULT_MAX_BODY_BYTES: u64 = 100 * 1024 * 1024;
/// Default page size for the OData list endpoints.
pub const DEFAULT_LIST_TOP: usize = 50;
/// Maximum page size accepted by the OData list endpoints.
pub const MAX_LIST_TOP: usize = 100;
/// Default ceiling for cached OAuth2 access tokens (5 minutes).
pub const DEFAULT_TOKEN_CACHE_TTL_SECS: u64 = 300;
/// Default capacity of the OAuth2 access-token cache.
pub const DEFAULT_TOKEN_CACHE_CAPACITY: usize = 10_000;
/// Default TTL before an unlinked custom plugin becomes garbage-collectable.
pub const DEFAULT_PLUGIN_GC_TTL_SECS: u64 = 30 * 24 * 60 * 60;

/// Server-side request forgery prevention switches.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct SsrfPolicy {
    /// When `true`, upstream endpoints resolving to loopback, private,
    /// link-local or otherwise non-public addresses are refused before the
    /// connection is dialled.
    pub enabled: bool,
}


/// Configuration of the OAGW gear.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct OagwConfig {
    /// Overall budget for a proxied request (connection + response headers).
    /// Also the source of the connection and idle timeouts.
    pub proxy_timeout_secs: u64,
    /// Additional grace period applied while streaming an upstream response
    /// body once the headers have been received.
    pub idle_timeout_secs: u64,
    /// Whether plaintext (`http`) upstream endpoints may be dialled.
    ///
    /// `false` (the default) enforces the HTTPS-only posture of
    /// `cpt-cf-oagw-constraint-https-only`: `http` endpoints are rejected at
    /// create time and `https` endpoints are dialled as usual.
    pub allow_http_upstream: bool,
    /// SSRF prevention switches.
    pub ssrf_policy: SsrfPolicy,
    /// Hard limit on a proxied request body, in bytes.
    pub max_body_bytes: u64,
    /// Default `$top` for list endpoints.
    pub list_top_default: usize,
    /// Maximum `$top` accepted for list endpoints.
    pub list_top_max: usize,
    /// Ceiling for cached OAuth2 access tokens, in seconds.
    pub token_cache_ttl_secs: u64,
    /// Maximum number of cached OAuth2 access tokens.
    pub token_cache_capacity: usize,
    /// Seconds after which an unlinked custom plugin may be collected.
    pub plugin_gc_ttl_secs: u64,
    /// Connection establishment budget, in seconds. Defaults to
    /// `proxy_timeout_secs` when unset.
    pub connect_timeout_secs: Option<u64>,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: 30,
            idle_timeout_secs: 300,
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicy::default(),
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            list_top_default: DEFAULT_LIST_TOP,
            list_top_max: MAX_LIST_TOP,
            token_cache_ttl_secs: DEFAULT_TOKEN_CACHE_TTL_SECS,
            token_cache_capacity: DEFAULT_TOKEN_CACHE_CAPACITY,
            plugin_gc_ttl_secs: DEFAULT_PLUGIN_GC_TTL_SECS,
            connect_timeout_secs: None,
        }
    }
}

impl OagwConfig {
    /// Connection establishment budget.
    #[must_use]
    pub fn connect_timeout(&self) -> Duration {
        Duration::from_secs(self.connect_timeout_secs.unwrap_or(self.proxy_timeout_secs))
    }

    /// Overall request budget.
    #[must_use]
    pub fn request_timeout(&self) -> Duration {
        Duration::from_secs(self.proxy_timeout_secs)
    }

    /// Idle (streaming) budget applied after response headers arrive.
    #[must_use]
    pub fn idle_timeout(&self) -> Duration {
        Duration::from_secs(self.idle_timeout_secs)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn defaults_enforce_https_only() {
        let cfg = OagwConfig::default();
        assert!(!cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.enabled);
        assert_eq!(cfg.max_body_bytes, DEFAULT_MAX_BODY_BYTES);
    }

    #[test]
    fn deserializes_e2e_config_shape() {
        let yaml = r#"
proxy_timeout_secs: 2
allow_http_upstream: true
ssrf_policy:
  enabled: false
"#;
        let cfg: OagwConfig = serde_yaml_shim_parse(yaml_to_json(yaml));
        assert_eq!(cfg.proxy_timeout_secs, 2);
        assert!(cfg.allow_http_upstream);
        assert_eq!(cfg.connect_timeout(), Duration::from_secs(2));
    }

    fn yaml_to_json(_yaml: &str) -> serde_json::Value {
        serde_json::json!({
            "proxy_timeout_secs": 2,
            "allow_http_upstream": true,
            "ssrf_policy": { "enabled": false }
        })
    }

    fn serde_yaml_shim_parse(value: serde_json::Value) -> OagwConfig {
        serde_json::from_value(value).expect("config should deserialize")
    }
}
