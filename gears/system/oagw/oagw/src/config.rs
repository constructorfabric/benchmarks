//! Gear-level configuration for OAGW.

use serde::Deserialize;

/// Default proxy timeout in seconds (per-request, applied to the whole
/// upstream round trip).
pub const DEFAULT_PROXY_TIMEOUT_SECS: u64 = 30;
/// Default ceiling for the OAuth2 token cache TTL (5 minutes, see
/// ADR-0008).
pub const DEFAULT_TOKEN_CACHE_TTL_SECS: u64 = 300;
/// Default maximum number of entries in the OAuth2 token cache (ADR-0008).
pub const DEFAULT_TOKEN_CACHE_CAPACITY: usize = 10_000;
/// Default hard body-size limit (100MB, DESIGN constraint
/// `cpt-cf-oagw-constraint-body-limit`).
pub const DEFAULT_MAX_BODY_BYTES: usize = 100 * 1024 * 1024;

/// OAGW gear configuration, loaded from the `oagw.config` section of the
/// host config.
///
/// Parsing is tolerant: the section may carry any subset of keys; missing
/// keys fall back to the defaults below (matching `config/e2e-local.yaml`).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct OagwConfig {
    /// Per-request timeout for the upstream round trip (seconds).
    pub proxy_timeout_secs: u64,
    /// Permit `http://` upstream schemes. Off by default (HTTPS-only,
    /// SSRF prevention); the e2e configuration enables it for local mocks.
    pub allow_http_upstream: bool,
    /// SSRF policy knobs (DNS pinning / segment controls are future work;
    /// the switch gates validation hardening).
    pub ssrf_policy: SsrfPolicyConfig,
    /// Ceiling for cached OAuth2 access-token TTL in seconds (ADR-0008).
    pub token_cache_ttl_secs: u64,
    /// Maximum entries in the OAuth2 token cache (ADR-0008).
    pub token_cache_capacity: usize,
    /// Hard body-size limit in bytes; requests larger than this are
    /// rejected with 413 before buffering.
    pub max_body_bytes: usize,
}

/// SSRF policy configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SsrfPolicyConfig {
    /// Master switch for SSRF hardening. When `true`, resolution refuses
    /// private/loopback ranges unless explicitly allowed (DNS pinning is
    /// future work; the flag gates the checks that exist today).
    pub enabled: bool,
}

impl Default for SsrfPolicyConfig {
    fn default() -> Self {
        Self { enabled: false }
    }
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
    /// The resolved proxy timeout as a [`std::time::Duration`].
    #[must_use]
    pub fn proxy_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.proxy_timeout_secs.max(1))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_e2e_contract() {
        let cfg = OagwConfig::default();
        assert_eq!(cfg.proxy_timeout_secs, DEFAULT_PROXY_TIMEOUT_SECS);
        assert!(!cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.enabled);
        assert_eq!(cfg.token_cache_ttl_secs, 300);
        assert_eq!(cfg.token_cache_capacity, 10_000);
        assert_eq!(cfg.max_body_bytes, 100 * 1024 * 1024);
    }

    #[test]
    fn parses_e2e_yaml_section() {
        // Shape of the `oagw.config` section in config/e2e-local.yaml.
        let json = serde_json::json!({
            "proxy_timeout_secs": 2,
            "allow_http_upstream": true,
            "ssrf_policy": { "enabled": false }
        });
        let cfg: OagwConfig = serde_json::from_value(json).unwrap();
        assert_eq!(cfg.proxy_timeout_secs, 2);
        assert!(cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.enabled);
    }

    #[test]
    fn parses_partial_section_with_defaults() {
        let cfg: OagwConfig = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(cfg.proxy_timeout(), std::time::Duration::from_secs(30));
        assert_eq!(cfg.max_body_bytes, DEFAULT_MAX_BODY_BYTES);
    }
}
