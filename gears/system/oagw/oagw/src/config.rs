// Created: 2026-08-29 by Constructor Tech
//! Gear configuration (`OagwConfig`).
//!
//! The struct is `#[serde(default)]` **without** `deny_unknown_fields`: the run
//! configuration supplies only a subset of the keys (`proxy_timeout_secs`,
//! `allow_http_upstream`, `ssrf_policy.enabled`) and must parse. Unknown keys
//! are ignored so the gear keeps working when new knobs appear in the config
//! file.

use serde::Deserialize;

/// Server-side request forgery guard knobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct SsrfPolicy {
    /// Reject upstream targets that resolve to private / loopback /
    /// link-local / unique-local addresses.
    ///
    /// Defaults to `true`: an outbound gateway that forwarded to loopback by
    /// default would be an open SSRF redirector. Deployments that must reach
    /// private ranges say so with `allow_private_addresses`.
    pub enabled: bool,
    /// Explicit escape hatch for deployments that must reach private ranges.
    pub allow_private_addresses: bool,
}

impl Default for SsrfPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            allow_private_addresses: false,
        }
    }
}

/// Configuration for the `oagw` outbound API gateway gear.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct OagwConfig {
    /// Wall-clock budget for a single proxied request (upstream call included).
    pub proxy_timeout_secs: u64,
    /// `HTTPS`-only upstreams unless this is explicitly `true`.
    ///
    /// Security: plaintext upstreams are rejected with `ProtocolError` while
    /// this stays `false`.
    pub allow_http_upstream: bool,
    /// SSRF guard configuration.
    pub ssrf_policy: SsrfPolicy,
    /// Maximum TTL handed to a cached OAuth2 token when the IdP omits `expires_in`.
    pub token_cache_ttl_secs: u64,
    /// OAuth2 token cache capacity (entries).
    pub token_cache_capacity: usize,
    /// Data-plane L1 effective-config cache capacity (entries).
    pub hot_cache_capacity: usize,
    /// TCP connect timeout for the upstream leg.
    pub connect_timeout_secs: u64,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: 30,
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicy::default(),
            token_cache_ttl_secs: 300,
            token_cache_capacity: 10_000,
            hot_cache_capacity: 1_000,
            connect_timeout_secs: 5,
        }
    }
}

impl OagwConfig {
    /// `proxy_timeout_secs` as a `std::time::Duration`.
    #[must_use]
    pub fn proxy_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.proxy_timeout_secs)
    }

    /// `connect_timeout_secs` as a `std::time::Duration`.
    #[must_use]
    pub fn connect_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.connect_timeout_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_contract() {
        let cfg = OagwConfig::default();
        assert_eq!(cfg.proxy_timeout_secs, 30);
        assert!(!cfg.allow_http_upstream);
        assert!(cfg.ssrf_policy.enabled, "SSRF protection is on by default");
        assert!(!cfg.ssrf_policy.allow_private_addresses);
        assert_eq!(cfg.token_cache_ttl_secs, 300);
        assert_eq!(cfg.token_cache_capacity, 10_000);
        assert_eq!(cfg.hot_cache_capacity, 1_000);
        assert_eq!(cfg.connect_timeout_secs, 5);
    }

    #[test]
    fn parses_partial_run_config() {
        let raw = serde_json::json!({
            "proxy_timeout_secs": 2,
            "allow_http_upstream": true,
            "ssrf_policy": { "enabled": false }
        });
        let cfg: OagwConfig = serde_json::from_value(raw).expect("partial config must parse");
        assert!(
            !cfg.ssrf_policy.enabled,
            "an explicit opt-out must be honoured"
        );
        assert_eq!(cfg.proxy_timeout_secs, 2);
        assert!(cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.enabled);
        assert_eq!(cfg.token_cache_ttl_secs, 300);
    }

    #[test]
    fn ignores_unknown_keys() {
        let raw = serde_json::json!({ "not_a_real_key": 1, "proxy_timeout_secs": 7 });
        let cfg: OagwConfig = serde_json::from_value(raw).expect("unknown keys are ignored");
        assert_eq!(cfg.proxy_timeout_secs, 7);
    }
}
