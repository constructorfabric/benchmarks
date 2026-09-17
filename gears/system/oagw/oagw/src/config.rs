//! Gear-level configuration for the OAGW gear.
//!
//! Config lives under `oagw.config` in the server YAML. The e2e config
//! provides:
//!
//! ```yaml
//! oagw:
//!   config:
//!     proxy_timeout_secs: 2
//!     allow_http_upstream: true
//!     ssrf_policy:
//!       enabled: false
//! ```
//!
//! All keys are snake_case with `deny_unknown_fields` so typos fail loudly.

use serde::Deserialize;
use std::time::Duration;

/// SSRF protection policy. Currently a master switch; when enabled the data
/// plane only allows HTTPS schemes for upstream endpoints.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SsrfPolicyConfig {
    /// Whether SSRF protections are active. When `true`, plaintext HTTP
    /// upstream endpoints are rejected by the data plane regardless of
    /// [`OagwConfig::allow_http_upstream`].
    #[serde(default)]
    pub enabled: bool,
}

/// Configuration for the OAGW gear.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OagwConfig {
    /// Total timeout applied to each proxied request (seconds).
    #[serde(default = "default_proxy_timeout_secs")]
    pub proxy_timeout_secs: u64,

    /// Allow plaintext `http://` upstream endpoints. Test-only escape hatch;
    /// will be rejected by the data plane when `ssrf_policy.enabled` is
    /// `true` or under FIPS builds.
    #[serde(default)]
    pub allow_http_upstream: bool,

    /// SSRF protection policy.
    #[serde(default)]
    pub ssrf_policy: SsrfPolicyConfig,

    /// Ceiling for cached OAuth2 access token TTL (seconds). Actual TTL is
    /// `min(config_ttl, expires_in - 30s)` per ADR-0008.
    #[serde(default = "default_token_cache_ttl_secs")]
    pub token_cache_ttl_secs: u64,

    /// Maximum entries in the OAuth2 token cache (ADR-0008).
    #[serde(default = "default_token_cache_capacity")]
    pub token_cache_capacity: usize,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: default_proxy_timeout_secs(),
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicyConfig::default(),
            token_cache_ttl_secs: default_token_cache_ttl_secs(),
            token_cache_capacity: default_token_cache_capacity(),
        }
    }
}

fn default_proxy_timeout_secs() -> u64 {
    30
}

fn default_token_cache_ttl_secs() -> u64 {
    300 // 5 minutes (ADR-0008)
}

fn default_token_cache_capacity() -> usize {
    10_000 // ADR-0008
}

impl OagwConfig {
    /// The proxy timeout as a `Duration`.
    #[must_use]
    pub fn proxy_timeout(&self) -> Duration {
        Duration::from_secs(self.proxy_timeout_secs)
    }

    /// TTL for OAuth2 token cache entries.
    #[must_use]
    pub fn token_cache_ttl(&self) -> Duration {
        Duration::from_secs(self.token_cache_ttl_secs)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_adr() {
        let cfg = OagwConfig::default();
        assert_eq!(cfg.proxy_timeout_secs, 30);
        assert_eq!(cfg.token_cache_ttl_secs, 300);
        assert_eq!(cfg.token_cache_capacity, 10_000);
        assert!(!cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.enabled);
    }

    #[test]
    fn parses_e2e_shape() {
        let json = r#"{
            "proxy_timeout_secs": 2,
            "allow_http_upstream": true,
            "ssrf_policy": { "enabled": false }
        }"#;
        let cfg: OagwConfig = serde_json::from_str(json).expect("valid config");
        assert_eq!(cfg.proxy_timeout_secs, 2);
        assert!(cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.enabled);
    }

    #[test]
    fn rejects_unknown_fields() {
        let json = r#"{"proxy_timeout_secs": 2, "bogus_key": true}"#;
        assert!(serde_json::from_str::<OagwConfig>(json).is_err());
    }
}
