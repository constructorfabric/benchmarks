//! Gear configuration, parsed from the `gears.oagw.config` section.

use serde::Deserialize;

/// Default proxy timeout in seconds.
const DEFAULT_PROXY_TIMEOUT_SECS: u64 = 30;
/// Default `OAuth2` token cache capacity.
const DEFAULT_TOKEN_CACHE_CAPACITY: usize = 1024;
/// Default outbound connection timeout in milliseconds.
const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 5_000;
/// Maximum proxied request body, in bytes.
pub const MAX_BODY_BYTES: usize = 100 * 1024 * 1024;

/// `SSRF` guard policy.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
pub struct SsrfPolicy {
    /// Whether `SSRF` host filtering is applied before dialling.
    #[serde(default)]
    pub enabled: bool,
}

/// Gear configuration.
///
/// Deliberately lenient: unknown members of the `gears.oagw.config` object are
/// ignored so that operator-side settings this gear does not model (for
/// example `ssrf_policy`) never block startup.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct OagwConfig {
    /// Whether a plaintext (`http`) connection may be made to an upstream.
    ///
    /// The schema admits only `https|wss|wt|grpc` endpoint schemes; this flag
    /// governs whether a non-`TLS` connection is actually opened. It exists so
    /// test and lab deployments can point the gateway at local `HTTP`
    /// upstreams.
    #[serde(default)]
    pub allow_http_upstream: bool,
    /// Upstream call timeout, in seconds.
    #[serde(default = "default_proxy_timeout_secs")]
    pub proxy_timeout_secs: u64,
    /// `OAuth2` token cache capacity.
    #[serde(default = "default_token_cache_capacity")]
    pub token_cache_capacity: usize,
    /// `OAuth2` token cache `TTL` floor, in seconds.
    #[serde(default = "default_token_cache_ttl_secs")]
    pub token_cache_ttl_secs: u64,
    /// Outbound connection timeout, in milliseconds.
    #[serde(default = "default_connect_timeout_ms")]
    pub connect_timeout_ms: u64,
    /// `SSRF` guard policy.
    #[serde(default)]
    pub ssrf_policy: SsrfPolicy,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            allow_http_upstream: false,
            proxy_timeout_secs: default_proxy_timeout_secs(),
            token_cache_capacity: default_token_cache_capacity(),
            token_cache_ttl_secs: default_token_cache_ttl_secs(),
            connect_timeout_ms: default_connect_timeout_ms(),
            ssrf_policy: SsrfPolicy::default(),
        }
    }
}

fn default_proxy_timeout_secs() -> u64 {
    DEFAULT_PROXY_TIMEOUT_SECS
}

fn default_token_cache_capacity() -> usize {
    DEFAULT_TOKEN_CACHE_CAPACITY
}

fn default_token_cache_ttl_secs() -> u64 {
    300
}

fn default_connect_timeout_ms() -> u64 {
    DEFAULT_CONNECT_TIMEOUT_MS
}

impl OagwConfig {
    /// The proxy timeout as a duration.
    #[must_use]
    pub fn proxy_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.proxy_timeout_secs.max(1))
    }

    /// The connect timeout as a duration.
    #[must_use]
    pub fn connect_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.connect_timeout_ms.max(1))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn defaults_apply() {
        let config = OagwConfig::default();
        assert!(!config.allow_http_upstream);
        assert_eq!(config.proxy_timeout_secs, DEFAULT_PROXY_TIMEOUT_SECS);
        assert_eq!(config.token_cache_capacity, DEFAULT_TOKEN_CACHE_CAPACITY);
        assert!(!config.ssrf_policy.enabled);
    }

    #[test]
    fn parses_from_gear_config_section() {
        let text = r#"
            {
              "proxy_timeout_secs": 2,
              "allow_http_upstream": true,
              "ssrf_policy": {"enabled": false}
            }
        "#;
        let config: OagwConfig = serde_json::from_str(text).unwrap();
        assert!(config.allow_http_upstream);
        assert_eq!(config.proxy_timeout_secs, 2);
    }

    #[test]
    fn ignores_unknown_members() {
        let config: OagwConfig = serde_json::from_str("{\"unknown_setting\": 1}").unwrap();
        assert_eq!(config, OagwConfig::default());
    }

    #[test]
    fn timeouts_are_bounded() {
        let config = OagwConfig {
            proxy_timeout_secs: 0,
            ..OagwConfig::default()
        };
        assert_eq!(config.proxy_timeout(), std::time::Duration::from_secs(1));
    }
}
