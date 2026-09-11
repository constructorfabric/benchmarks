//! Gear-level configuration for the `oagw` gear.
//!
//! Deserialised from the `gears.oagw.config` node of the host configuration
//! (`config/e2e-local.yaml` seeds `proxy_timeout_secs`, `allow_http_upstream`
//! and `ssrf_policy.enabled`).

use serde::{Deserialize, Serialize};

/// Server-Side Request Forgery posture for outbound dials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SsrfPolicy {
    /// When false, no SSRF filtering is applied to endpoint hosts. On by
    /// default: the gateway dials operator-configured hosts only.
    #[serde(default = "default_ssrf_enabled")]
    pub enabled: bool,
    /// CIDR ranges the data plane refuses to dial when SSRF filtering is on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub denied_cidrs: Vec<String>,
}

impl Default for SsrfPolicy {
    fn default() -> Self {
        Self { enabled: default_ssrf_enabled(), denied_cidrs: Vec::new() }
    }
}

fn default_ssrf_enabled() -> bool {
    true
}

/// Token-cache tuning (ADR 0008).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TokenCacheConfig {
    /// Upper bound of cached token entries.
    #[serde(default = "default_token_cache_capacity")]
    pub capacity: usize,
    /// Default TTL, in seconds, applied when a plugin config does not set one.
    #[serde(default = "default_token_cache_ttl_secs")]
    pub ttl_secs: u64,
}

impl Default for TokenCacheConfig {
    fn default() -> Self {
        Self {
            capacity: default_token_cache_capacity(),
            ttl_secs: default_token_cache_ttl_secs(),
        }
    }
}

fn default_token_cache_capacity() -> usize {
    10_000
}

fn default_token_cache_ttl_secs() -> u64 {
    300
}

/// Gear-level configuration for the `oagw` gear.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OagwConfig {
    /// Per-attempt timeout applied to an outbound upstream request.
    #[serde(default = "default_proxy_timeout_secs")]
    pub proxy_timeout_secs: u64,
    /// When `true`, `http` endpoints may actually be dialled in plaintext.
    ///
    /// The scheme *field* always accepts `http`; this flag only governs whether
    /// a plaintext connection is established (DESIGN `cpt-cf-oagw-constraint-https-only`).
    #[serde(default)]
    pub allow_http_upstream: bool,
    /// SSRF posture for outbound dials.
    #[serde(default)]
    pub ssrf_policy: SsrfPolicy,
    /// OAuth2 token cache (ADR 0008).
    #[serde(default)]
    pub token_cache: TokenCacheConfig,
    /// Hard cap on a proxied request body, in bytes.
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: usize,
    /// Number of pooled connections kept per upstream endpoint.
    #[serde(default = "default_pool_size")]
    pub pool_size: usize,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: default_proxy_timeout_secs(),
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicy::default(),
            token_cache: TokenCacheConfig::default(),
            max_body_bytes: default_max_body_bytes(),
            pool_size: default_pool_size(),
        }
    }
}

fn default_proxy_timeout_secs() -> u64 {
    30
}

fn default_max_body_bytes() -> usize {
    100 * 1024 * 1024
}

fn default_pool_size() -> usize {
    128
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_documented_https_only_posture() {
        let cfg = OagwConfig::default();
        assert_eq!(cfg.proxy_timeout_secs, 30);
        assert!(!cfg.allow_http_upstream, "plaintext dials are off by default");
        assert!(cfg.ssrf_policy.enabled, "SSRF filtering is on by default");
        assert_eq!(cfg.token_cache.ttl_secs, 300);
        assert_eq!(cfg.token_cache.capacity, 10_000);
        assert_eq!(cfg.max_body_bytes, 100 * 1024 * 1024);
    }

    #[test]
    fn an_empty_config_node_deserialises_to_defaults() {
        let cfg: OagwConfig = serde_json::from_str("{}").expect("empty object parses");
        assert_eq!(cfg, OagwConfig::default());
    }

    #[test]
    fn the_e2e_config_node_deserialises() {
        let cfg: OagwConfig = serde_json::from_str(
            r#"{"proxy_timeout_secs":2,"allow_http_upstream":true,
                "ssrf_policy":{"enabled":false}}"#,
        )
        .expect("e2e config node parses");
        assert_eq!(cfg.proxy_timeout_secs, 2);
        assert!(cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.enabled);
    }

    #[test]
    fn unknown_config_keys_are_rejected() {
        assert!(serde_json::from_str::<OagwConfig>(r#"{"nope":1}"#).is_err());
    }
}
