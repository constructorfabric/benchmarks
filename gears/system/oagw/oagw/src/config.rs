//! Gear configuration for the OAGW module (`gears.oagw.config`) (feature
//! `cpt-cf-oagw-feature-gear-foundation`).
//!
//! The configuration is resolved from the `gears.oagw.config` YAML block via
//! `GearCtx::config_or_default`, which falls back to [`OagwConfig::default`]
//! for absent keys (flow `cpt-cf-oagw-flow-gear-foundation-boot`, steps
//! `inst-gf-boot-config`/`inst-gf-cfg-*`).  Every runtime knob owned by this
//! gear lives here so the resolved [`OagwConfig`] is the single source of
//! runtime defaults for the Control Plane and the Data Plane (DoD
//! `cpt-cf-oagw-dod-gear-foundation-config`, algorithm
//! `cpt-cf-oagw-algo-gear-foundation-load-config`).
//!
//! # Contract
//!
//! | Key | Default | Description |
//! |-----|---------|-------------|
//! | `proxy_timeout_secs` | `2` | Upstream request timeout on the Data Plane hot path |
//! | `allow_http_upstream` | `false` | Opt-in to plaintext HTTP upstreams (testing only) |
//! | `ssrf_policy.enabled` | `true` | SSRF guard (scheme allowlist, hostname validation, header stripping) |
//! | `token_cache_ttl_secs` | `300` | Ceiling TTL for the OAuth2 token cache |
//! | `token_cache_capacity` | `10000` | Maximum entries in the OAuth2 token cache |

use serde::{Deserialize, Serialize};

/// SSRF guard policy applied to upstream endpoint URLs at the configuration
/// boundary and on the Data Plane hot path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SsrfPolicy {
    /// Whether the SSRF guard runs: scheme allowlist, RFC 1123 hostname
    /// validation, and hop-by-hop header stripping.  Enabled by default.
    pub enabled: bool,
}

impl Default for SsrfPolicy {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Resolved configuration for the OAGW gear.
///
/// All fields carry `serde(default, deny_unknown_fields)` so that partial
/// `gears.oagw.config` YAML blocks deserialize with defaults filling the
/// remainder and unknown keys are rejected at boot time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OagwConfig {
    /// Upstream request timeout (seconds) on the Data Plane hot path.
    pub proxy_timeout_secs: u64,
    /// Opt-in to plaintext HTTP upstreams (testing only).  Default `false`
    /// keeps the outbound surface HTTPS-only (SSRF mitigation layer).
    pub allow_http_upstream: bool,
    /// SSRF guard policy for upstream endpoint URLs.
    pub ssrf_policy: SsrfPolicy,
    /// Ceiling TTL (seconds) for the OAuth2 token cache.
    pub token_cache_ttl_secs: u64,
    /// Maximum number of entries in the OAuth2 token cache.
    pub token_cache_capacity: usize,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: 2,
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicy::default(),
            token_cache_ttl_secs: 300,
            token_cache_capacity: 10_000,
        }
    }
}

impl OagwConfig {
    /// Validates the resolved configuration, rejecting values that would be
    /// unsafe or incoherent at runtime.
    ///
    /// # Errors
    /// Returns a descriptive error for the first invalid field:
    /// - `proxy_timeout_secs == 0` would make every proxy request time out
    ///   immediately on the hot path;
    /// - `token_cache_ttl_secs == 0` would disable the ceiling TTL;
    /// - `token_cache_capacity == 0` would make the token cache unusable.
    pub fn validate(&self) -> Result<(), String> {
        if self.proxy_timeout_secs == 0 {
            return Err("proxy_timeout_secs must be >= 1".to_owned());
        }
        if self.token_cache_ttl_secs == 0 {
            return Err("token_cache_ttl_secs must be >= 1".to_owned());
        }
        if self.token_cache_capacity == 0 {
            return Err("token_cache_capacity must be >= 1".to_owned());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_applies_documented_defaults() {
        let cfg = OagwConfig::default();
        assert_eq!(cfg.proxy_timeout_secs, 2);
        assert!(!cfg.allow_http_upstream);
        assert!(cfg.ssrf_policy.enabled);
        assert_eq!(cfg.token_cache_ttl_secs, 300);
        assert_eq!(cfg.token_cache_capacity, 10_000);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn partial_block_deserializes_with_defaults() {
        // Only `proxy_timeout_secs` present: everything else is defaulted.
        let json = serde_json::json!({ "proxy_timeout_secs": 5 });
        let cfg: OagwConfig = serde_json::from_value(json).expect("partial config");
        assert_eq!(cfg.proxy_timeout_secs, 5);
        assert!(!cfg.allow_http_upstream);
        assert!(cfg.ssrf_policy.enabled);
        assert_eq!(cfg.token_cache_ttl_secs, 300);
        assert_eq!(cfg.token_cache_capacity, 10_000);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn e2e_local_block_deserializes() {
        // Mirrors the `gears.oagw.config` block from `e2e-local.yaml`:
        // allow_http_upstream + ssrf_policy.enabled are overridden, the rest
        // is defaulted.
        let json = serde_json::json!({
            "proxy_timeout_secs": 2,
            "allow_http_upstream": true,
            "ssrf_policy": { "enabled": false },
        });
        let cfg: OagwConfig = serde_json::from_value(json).expect("e2e-local config");
        assert_eq!(cfg.proxy_timeout_secs, 2);
        assert!(cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.enabled);
        assert_eq!(cfg.token_cache_ttl_secs, 300);
        assert_eq!(cfg.token_cache_capacity, 10_000);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn ssrf_policy_is_self_describing() {
        // An absent `ssrf_policy` object defaults to enabled.
        let json = serde_json::json!({});
        let cfg: OagwConfig = serde_json::from_value(json).expect("empty config");
        assert!(cfg.ssrf_policy.enabled);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let json = serde_json::json!({ "bogus_key": 1 });
        let err = serde_json::from_value::<OagwConfig>(json).expect_err("deny_unknown_fields");
        assert!(err.to_string().contains("bogus_key"));
    }

    #[test]
    fn validate_rejects_zero_timeouts_and_capacity() {
        let cfg = OagwConfig {
            proxy_timeout_secs: 0,
            ..Default::default()
        };
        assert!(cfg.validate().is_err());

        let cfg = OagwConfig {
            token_cache_ttl_secs: 0,
            ..Default::default()
        };
        assert!(cfg.validate().is_err());

        let cfg = OagwConfig {
            token_cache_capacity: 0,
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn serializes_round_trip() {
        let cfg = OagwConfig::default();
        let json = serde_json::to_value(&cfg).expect("serialize");
        let back: OagwConfig = serde_json::from_value(json).expect("deserialize");
        assert_eq!(cfg, back);
    }
}
