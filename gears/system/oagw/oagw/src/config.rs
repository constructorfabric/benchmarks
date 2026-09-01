//! Gear configuration for the OAGW (outbound API gateway).
//!
//! The config section is deserialized from `gears.oagw.config` and every
//! field carries a safe default so an unconfigured gear still boots (see
//! [`GearCtx::config_or_default`]).

use serde::Deserialize;
use std::time::Duration;

/// `OAuth2` client-credentials token cache configuration (ADR-0008).
///
/// These are gear-level knobs bundled into a single struct and threaded
/// through to the auth plugin registries.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TokenCacheConfig {
    /// Ceiling for cached access token TTL (seconds). The actual entry TTL is
    /// `min(config_ttl, expires_in - 30s safety margin)` as reported by the
    /// `IdP`. Kept short because there is no cache-invalidation mechanism yet.
    pub ttl_secs: u64,
    /// Maximum number of entries in the token cache.
    pub capacity: usize,
}

impl Default for TokenCacheConfig {
    fn default() -> Self {
        Self {
            ttl_secs: 300,
            capacity: 10_000,
        }
    }
}

impl TokenCacheConfig {
    /// The configured cache entry TTL as a [`Duration`].
    #[must_use]
    pub fn ttl(&self) -> Duration {
        Duration::from_secs(self.ttl_secs)
    }
}

/// SSRF protection policy.
///
/// `enabled` gates DNS/connection safety checks for upstream hosts. The e2e
/// harness runs with it disabled so tests can target plain `http://`
/// localhost mock servers; production deployments are expected to leave it
/// on (the default).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SsrfPolicyConfig {
    /// Whether SSRF enforcement is enabled.
    pub enabled: bool,
}

impl Default for SsrfPolicyConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Top-level OAGW gear configuration.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct OagwConfig {
    /// Total proxy request timeout in seconds (connect + request + idle).
    pub proxy_timeout_secs: u64,
    /// Whether plain-`http` upstream schemes are allowed. `false` (the
    /// default) restricts upstreams to `https`/`wss` per the MVP
    /// HTTPS-only constraint.
    pub allow_http_upstream: bool,
    /// SSRF protection policy.
    pub ssrf_policy: SsrfPolicyConfig,
    /// `OAuth2` client-credentials token cache tuning (ADR-0008).
    pub token_cache: TokenCacheConfig,
    /// Maximum upstream response body accepted before buffering is aborted
    /// (100 MiB hard limit per the body-size constraint).
    pub max_body_bytes: usize,
    /// Maximum number of upstreams a single tenant may manage. Guards
    /// against accidental unbounded memory growth in the in-memory control
    /// plane.
    pub max_upstreams_per_tenant: usize,
    /// Maximum number of routes per upstream.
    pub max_routes_per_upstream: usize,
    /// Maximum number of plugins per tenant.
    pub max_plugins_per_tenant: usize,
    /// Listing `$top` ceiling for management list endpoints.
    pub listing_max_top: u32,
    /// Listing `$top` default when the caller omits it.
    pub listing_default_top: u32,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: 2,
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicyConfig::default(),
            token_cache: TokenCacheConfig::default(),
            max_body_bytes: 100 * 1024 * 1024,
            max_upstreams_per_tenant: 500,
            max_routes_per_upstream: 500,
            max_plugins_per_tenant: 200,
            listing_max_top: 100,
            listing_default_top: 50,
        }
    }
}

impl OagwConfig {
    /// The proxy timeout as a [`Duration`].
    #[must_use]
    pub fn proxy_timeout(&self) -> Duration {
        Duration::from_secs(self.proxy_timeout_secs)
    }

    /// Validate structural invariants; returns a human-readable list of
    /// problems (empty when valid).
    #[must_use]
    pub fn validation_errors(&self) -> Vec<String> {
        let mut errors = Vec::new();
        if self.proxy_timeout_secs == 0 {
            errors.push("proxy_timeout_secs must be > 0".to_owned());
        }
        if self.max_body_bytes == 0 {
            errors.push("max_body_bytes must be > 0".to_owned());
        }
        if self.listing_default_top == 0 || self.listing_default_top > self.listing_max_top {
            errors.push("listing_default_top must be in [1, listing_max_top]".to_owned());
        }
        errors
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn serde_defaults_match_documented_values() {
        let cfg = OagwConfig::default();
        assert_eq!(cfg.proxy_timeout_secs, 2);
        assert!(!cfg.allow_http_upstream);
        assert!(cfg.ssrf_policy.enabled);
        assert_eq!(cfg.token_cache.ttl_secs, 300);
        assert_eq!(cfg.token_cache.ttl(), Duration::from_mins(5));
        assert_eq!(cfg.token_cache.capacity, 10_000);
        assert_eq!(cfg.max_body_bytes, 100 * 1024 * 1024);
    }

    #[test]
    fn deserialize_roundtrip_with_documented_overrides() {
        let json = serde_json::json!({
            "proxy_timeout_secs": 2,
            "allow_http_upstream": true,
            "ssrf_policy": { "enabled": false },
            "token_cache": { "ttl_secs": 300, "capacity": 10000 }
        });
        let cfg: OagwConfig = serde_json::from_value(json).unwrap();
        assert_eq!(cfg.proxy_timeout_secs, 2);
        assert!(cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.enabled);
        assert_eq!(cfg.token_cache.ttl(), Duration::from_mins(5));
    }

    #[test]
    fn empty_config_deserializes_to_defaults() {
        let cfg: OagwConfig = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(cfg, OagwConfig::default());
    }
}
