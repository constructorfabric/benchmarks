// @cpt-begin:cpt-cf-oagw-dod-gear-foundation-config-defaults:p1:inst-config
//! Gear configuration for the outbound API gateway.
//!
//! Deserialized from the `gears.oagw.config` block of the server configuration.
//! Every field carries a default so an absent block still yields a usable gear.

use serde::Deserialize;

/// Default outbound proxy timeout in seconds.
const fn default_proxy_timeout_secs() -> u64 {
    30
}

/// Default `OAuth2` token-cache time to live, in seconds (ADR-0008).
const fn default_token_cache_ttl_secs() -> u64 {
    300
}

/// Default `OAuth2` token-cache capacity (ADR-0008).
const fn default_token_cache_capacity() -> usize {
    10_000
}

/// Server-side request forgery policy.
///
/// The checks always run; this flag decides whether a failure is enforced.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SsrfPolicyConfig {
    /// Whether an SSRF check failure rejects the request.
    pub enabled: bool,
}

impl Default for SsrfPolicyConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Gear-level configuration for `oagw`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct OagwConfig {
    /// Timeout applied to reaching an upstream response head, in seconds.
    ///
    /// It does not bound the lifetime of an established stream or WebSocket
    /// session; those are governed by their own lifecycle.
    pub proxy_timeout_secs: u64,
    /// Whether a plaintext upstream connection may actually be made.
    ///
    /// This is independent of which schemes the API accepts at create time.
    pub allow_http_upstream: bool,
    /// Server-side request forgery policy.
    pub ssrf_policy: SsrfPolicyConfig,
    /// Time to live for cached `OAuth2` client-credentials tokens, in seconds.
    pub token_cache_ttl_secs: u64,
    /// Maximum number of cached `OAuth2` client-credentials tokens.
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
// @cpt-end:cpt-cf-oagw-dod-gear-foundation-config-defaults:p1:inst-config

#[cfg(test)]
mod tests {
    use super::OagwConfig;

    #[test]
    fn defaults_apply_when_block_is_absent() {
        let cfg = OagwConfig::default();
        assert_eq!(cfg.proxy_timeout_secs, 30);
        assert!(!cfg.allow_http_upstream);
        assert!(cfg.ssrf_policy.enabled);
        assert_eq!(cfg.token_cache_ttl_secs, 300);
        assert_eq!(cfg.token_cache_capacity, 10_000);
    }

    #[test]
    fn graded_configuration_block_deserializes() {
        let value = serde_json::json!({
            "proxy_timeout_secs": 2,
            "allow_http_upstream": true,
            "ssrf_policy": { "enabled": false }
        });
        let cfg: OagwConfig = serde_json::from_value(value).expect("config parses");
        assert_eq!(cfg.proxy_timeout_secs, 2);
        assert!(cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.enabled);
        // Fields absent from the block still fall back to their defaults.
        assert_eq!(cfg.token_cache_ttl_secs, 300);
    }

    #[test]
    fn unknown_field_is_rejected() {
        let value = serde_json::json!({ "nope": 1 });
        assert!(serde_json::from_value::<OagwConfig>(value).is_err());
    }
}
