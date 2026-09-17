//! Gear configuration (`gears.oagw.config`).
//!
//! Loaded through [`toolkit::GearCtx::config_or_default`] so the gear boots
//! with safe defaults even when the operator supplies no configuration section.

use serde::{Deserialize, Serialize};

/// Hard ceiling on a proxied request body, per DESIGN §2.2 (100 MB).
pub const DEFAULT_MAX_BODY_BYTES: u64 = 100 * 1024 * 1024;
/// Default ceiling applied to a cached OAuth2 access token (5 minutes).
pub const DEFAULT_TOKEN_CACHE_TTL_SECS: u64 = 300;
/// Default number of cached OAuth2 access tokens.
pub const DEFAULT_TOKEN_CACHE_CAPACITY: usize = 10_000;
/// Default upstream round-trip timeout in seconds.
pub const DEFAULT_PROXY_TIMEOUT_SECS: u64 = 30;

/// SSRF posture. Disabled means the allowlist/DNS-pin checks are skipped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct SsrfPolicyConfig {
    /// Master switch. `false` skips every SSRF check.
    pub enabled: bool,
    /// IP ranges that are never dialled when SSRF enforcement is on.
    pub denied_segments: Vec<String>,
}

impl Default for SsrfPolicyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            denied_segments: Vec::new(),
        }
    }
}

/// Top-level `gears.oagw.config` document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct OagwConfig {
    /// Hard timeout applied to the whole upstream round trip.
    pub proxy_timeout_secs: u64,
    /// When `false`, a plaintext (`http` scheme) upstream is refused at proxy
    /// time. The scheme itself stays legal so configuration can be validated
    /// and stored regardless.
    pub allow_http_upstream: bool,
    /// SSRF posture.
    pub ssrf_policy: SsrfPolicyConfig,
    /// Ceiling for a cached OAuth2 access token TTL.
    pub token_cache_ttl_secs: u64,
    /// Maximum entries in the OAuth2 token cache.
    pub token_cache_capacity: usize,
    /// Maximum proxied request body size in bytes.
    pub max_body_bytes: u64,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: DEFAULT_PROXY_TIMEOUT_SECS,
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicyConfig::default(),
            token_cache_ttl_secs: DEFAULT_TOKEN_CACHE_TTL_SECS,
            token_cache_capacity: DEFAULT_TOKEN_CACHE_CAPACITY,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
        }
    }
}

impl OagwConfig {
    /// Proxy timeout as a [`std::time::Duration`].
    #[must_use]
    pub fn proxy_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.proxy_timeout_secs)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_design() {
        let cfg = OagwConfig::default();
        assert_eq!(cfg.proxy_timeout_secs, 30);
        assert!(!cfg.allow_http_upstream);
        assert!(cfg.ssrf_policy.enabled);
        assert_eq!(cfg.token_cache_ttl_secs, 300);
        assert_eq!(cfg.token_cache_capacity, 10_000);
        assert_eq!(cfg.max_body_bytes, DEFAULT_MAX_BODY_BYTES);
    }

    #[test]
    fn parses_snake_case_document() {
        let cfg: OagwConfig = serde_json::from_value(serde_json::json!({
            "proxy_timeout_secs": 2,
            "allow_http_upstream": true,
            "ssrf_policy": { "enabled": false }
        }))
        .expect("valid config document");
        assert_eq!(cfg.proxy_timeout_secs, 2);
        assert!(cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.enabled);
        assert_eq!(cfg.token_cache_ttl_secs, DEFAULT_TOKEN_CACHE_TTL_SECS);
    }
}
