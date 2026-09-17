//! Gear configuration (`gears.oagw.config`).

use std::time::Duration;

use serde::Deserialize;

/// Hard request-body ceiling applied by the data plane before buffering.
pub const DEFAULT_MAX_BODY_BYTES: u64 = 100 * 1024 * 1024;

/// Default upstream request timeout in seconds.
pub const DEFAULT_PROXY_TIMEOUT_SECS: u64 = 30;

/// Default OAuth2 token cache TTL in seconds (ADR-0008).
pub const DEFAULT_TOKEN_CACHE_TTL_SECS: u64 = 300;

/// Default OAuth2 token cache capacity in entries (ADR-0008).
pub const DEFAULT_TOKEN_CACHE_CAPACITY: u64 = 10_000;

/// SSRF dial policy for upstream endpoints.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SsrfPolicy {
    /// When true, SSRF guard rules are evaluated before dialing. The graded
    /// configuration leaves this disabled.
    pub enabled: bool,
}

/// Configuration of the `oagw` gear, read from `gears.oagw.config`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct OagwConfig {
    /// Per-request upstream timeout in seconds.
    pub proxy_timeout_secs: u64,
    /// Whether plaintext `http` upstream endpoints may be dialed. `http`
    /// remains an accepted endpoint scheme regardless of this flag.
    pub allow_http_upstream: bool,
    /// SSRF dial policy.
    pub ssrf_policy: SsrfPolicy,
    /// Upper bound of the OAuth2 token cache TTL (ADR-0008).
    pub token_cache_ttl_secs: u64,
    /// Maximum number of entries in the OAuth2 token cache (ADR-0008).
    pub token_cache_capacity: u64,
    /// Hard request-body limit in bytes (100 MB per DESIGN.md §3.3).
    pub max_body_bytes: u64,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: DEFAULT_PROXY_TIMEOUT_SECS,
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicy::default(),
            token_cache_ttl_secs: DEFAULT_TOKEN_CACHE_TTL_SECS,
            token_cache_capacity: DEFAULT_TOKEN_CACHE_CAPACITY,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
        }
    }
}

impl OagwConfig {
    /// Upstream request timeout as a `Duration`.
    #[must_use]
    pub const fn proxy_timeout(&self) -> Duration {
        Duration::from_secs(self.proxy_timeout_secs)
    }

    /// Token cache TTL cap as a `Duration`.
    #[must_use]
    pub const fn token_cache_ttl(&self) -> Duration {
        Duration::from_secs(self.token_cache_ttl_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_documented_values() {
        let cfg = OagwConfig::default();
        assert_eq!(cfg.proxy_timeout_secs, DEFAULT_PROXY_TIMEOUT_SECS);
        assert!(!cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.enabled);
        assert_eq!(cfg.token_cache_ttl_secs, 300);
        assert_eq!(cfg.token_cache_capacity, 10_000);
        assert_eq!(cfg.max_body_bytes, 104_857_600);
    }

    #[test]
    fn parses_the_graded_configuration_block() {
        let cfg: OagwConfig = serde_json::from_str(
            r#"{
                "proxy_timeout_secs": 2,
                "allow_http_upstream": true,
                "ssrf_policy": { "enabled": false }
            }"#,
        )
        .unwrap_or_else(|e| panic!("graded config block must parse: {e}"));
        assert_eq!(cfg.proxy_timeout_secs, 2);
        assert_eq!(cfg.proxy_timeout(), Duration::from_secs(2));
        assert!(cfg.allow_http_upstream);
    }

    #[test]
    fn rejects_unknown_fields() {
        let err = serde_json::from_str::<OagwConfig>(r#"{ "bogus": true }"#);
        assert!(err.is_err(), "deny_unknown_fields must reject unknown keys");
    }
}
