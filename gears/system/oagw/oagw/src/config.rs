//! OAGW gear configuration.
//!
//! Loaded from `gears.oagw.config` via [`toolkit::GearCtx::config_or_default`].
//! The struct is strictly validated: unknown keys are rejected
//! (`deny_unknown_fields`) and missing keys fall back to the documented
//! defaults.  The e2e configuration only sets `proxy_timeout_secs`,
//! `allow_http_upstream` and the nested `ssrf_policy.enabled`, so those keys
//! are what the wire contract must accept.

use serde::Deserialize;

/// Nested SSRF policy object (NOT a flat boolean — the e2e config uses
/// `ssrf_policy: { enabled: false }`).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct SsrfPolicy {
    /// Master switch. When `false` no outbound filtering is applied
    /// (test environments, e.g. `httpmock` on loopback).
    pub enabled: bool,
    /// Block private / loopback / link-local destinations up front
    /// (checked before connecting).
    pub block_private_networks: bool,
    /// Additional CIDR ranges always permitted (whitelist).
    pub allowed_cidrs: Vec<String>,
    /// CIDR ranges always denied (overrides `allowed_cidrs`).
    pub denied_cidrs: Vec<String>,
}

impl Default for SsrfPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            block_private_networks: true,
            allowed_cidrs: Vec::new(),
            denied_cidrs: Vec::new(),
        }
    }
}

/// Outbound API Gateway configuration.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct OagwConfig {
    /// Outbound proxy request/connect timeout in seconds.
    pub proxy_timeout_secs: u64,

    /// Permit plaintext `http`/`ws` upstream schemes.  The base schema only
    /// admits `https|wss|wt|grpc`; this flag widens the accepted scheme set
    /// for local testing (`httpmock`).
    pub allow_http_upstream: bool,

    /// SSRF policy (nested object — must stay nested for e2e config compat).
    pub ssrf_policy: SsrfPolicy,

    /// ADR-0008 token-cache TTL ceiling (seconds). Default 300.
    pub token_cache_ttl_secs: u64,

    /// ADR-0008 token-cache capacity (entries). Default 10 000.
    pub token_cache_capacity: usize,

    /// Max idle connections retained in the outbound pool.
    pub upstream_pool_max_idle: usize,

    /// Hard request body cap (bytes). Default 100 MiB.
    pub max_request_body_bytes: u64,

    /// Extra hop-by-hop header names stripped on every proxied request.
    pub strip_headers: Vec<String>,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: 30,
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicy::default(),
            token_cache_ttl_secs: 300,
            token_cache_capacity: 10_000,
            upstream_pool_max_idle: 64,
            max_request_body_bytes: 100 * 1024 * 1024,
            strip_headers: Vec::new(),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn default_config_matches_documented_defaults() {
        let cfg = OagwConfig::default();
        assert_eq!(cfg.proxy_timeout_secs, 30);
        assert!(!cfg.allow_http_upstream);
        assert!(cfg.ssrf_policy.enabled);
        assert_eq!(cfg.token_cache_ttl_secs, 300);
        assert_eq!(cfg.token_cache_capacity, 10_000);
        assert_eq!(cfg.max_request_body_bytes, 100 * 1024 * 1024);
    }

    #[test]
    fn e2e_shape_deserializes() {
        let raw = serde_json::json!({
            "proxy_timeout_secs": 2,
            "allow_http_upstream": true,
            "ssrf_policy": { "enabled": false }
        });
        let cfg: OagwConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(cfg.proxy_timeout_secs, 2);
        assert!(cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.enabled);
        // Unset keys fall back to defaults.
        assert_eq!(cfg.token_cache_ttl_secs, 300);
        assert_eq!(cfg.token_cache_capacity, 10_000);
    }

    #[test]
    fn adr0008_keys_deserialize() {
        let raw = serde_json::json!({
            "token_cache_ttl_secs": 60,
            "token_cache_capacity": 128
        });
        let cfg: OagwConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(cfg.token_cache_ttl_secs, 60);
        assert_eq!(cfg.token_cache_capacity, 128);
    }

    #[test]
    fn unknown_top_level_key_rejected() {
        let raw = serde_json::json!({ "not_a_real_key": true });
        assert!(serde_json::from_value::<OagwConfig>(raw).is_err());
    }

    #[test]
    fn unknown_ssrf_key_rejected() {
        let raw = serde_json::json!({ "ssrf_policy": { "bogus": 1 } });
        assert!(serde_json::from_value::<OagwConfig>(raw).is_err());
    }
}
