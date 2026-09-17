//! OAGW gear configuration.
//!
//! Loaded from the `gears.oagw.config` YAML block via
//! [`toolkit::GearCtx::config_or_default`]. Every field carries a documented
//! default so the gear boots with no configuration at all; the e2e profile
//! sets `proxy_timeout_secs`, `allow_http_upstream` and `ssrf_policy.enabled`.

use serde::{Deserialize, Serialize};

/// Hard maximum for proxied request bodies (100 MiB, per DESIGN "Body
/// Validation Rules"). Rejected with `413 PayloadTooLarge` before buffering.
pub const DEFAULT_MAX_BODY_BYTES: usize = 100 * 1024 * 1024;

/// SSRF protection policy configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SsrfPolicyConfig {
    /// When enabled, OAGW requires every upstream endpoint to resolve to a
    /// public (non-loopback, non-link-local, non-private) address. When
    /// disabled (default — the e2e profile exercises local/private upstreams),
    /// resolution results are accepted as configured.
    pub enabled: bool,
}

impl Default for SsrfPolicyConfig {
    fn default() -> Self {
        Self { enabled: false }
    }
}

/// Token cache configuration for the OAuth2 client-credentials auth plugin
/// (see ADR-0008: min(config_ttl, `expires_in - 30s`)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TokenCacheConfig {
    /// Ceiling for cached access-token TTL, in seconds (default 300 / 5 min).
    pub ttl_secs: u64,
    /// Maximum number of entries in the in-memory token cache (default 10_000).
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

/// OAGW gear configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct OagwConfig {
    /// Per-request timeout for outbound (proxy and OAuth token) requests, in
    /// seconds. Default 30; the e2e profile sets 2 so timeouts are observable.
    pub proxy_timeout_secs: u64,

    /// Allow proxying to plain-HTTP upstream endpoints. When `false` (default)
    /// only HTTPS/WSS/WT/gRPC (`scheme != http`) endpoints are shippable.
    pub allow_http_upstream: bool,

    /// SSRF protection policy (see [`SsrfPolicyConfig`]).
    pub ssrf_policy: SsrfPolicyConfig,

    /// Maximum request/response body size in bytes accepted by the proxy and
    /// forwarded to upstreams (hard limit 100 MiB; `413 PayloadTooLarge`).
    pub body_limit_bytes: usize,

    /// OAuth2 token cache configuration (see [`TokenCacheConfig`]).
    pub token_cache: TokenCacheConfig,

    /// Default inbound request timeout enforced before forwarding (seconds).
    /// Not a full client-retry: the gateway never re-issues client requests.
    pub request_timeout_secs: u64,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: 30,
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicyConfig::default(),
            body_limit_bytes: DEFAULT_MAX_BODY_BYTES,
            token_cache: TokenCacheConfig::default(),
            request_timeout_secs: 30,
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let cfg = OagwConfig::default();
        assert_eq!(cfg.proxy_timeout_secs, 30);
        assert!(!cfg.allow_http_upstream);
        assert_eq!(cfg.body_limit_bytes, DEFAULT_MAX_BODY_BYTES);
    }

    #[test]
    fn e2e_config_block_deserializes() {
        // JSON mirrors the `gears.oagw.config` block from config/e2e-local.yaml.
        let json = serde_json::json!({
            "proxy_timeout_secs": 2,
            "allow_http_upstream": true,
            "ssrf_policy": { "enabled": false }
        });
        let cfg: OagwConfig = serde_json::from_value(json).expect("valid config");
        assert_eq!(cfg.proxy_timeout_secs, 2);
        assert!(cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.enabled);
    }

    #[test]
    fn unknown_fields_rejected() {
        let json = serde_json::json!({ "not_a_real_field": true });
        assert!(serde_json::from_value::<OagwConfig>(json).is_err());
    }

    #[test]
    fn full_config_round_trips_with_all_blocks() {
        let json = serde_json::json!({
            "proxy_timeout_secs": 7,
            "allow_http_upstream": false,
            "ssrf_policy": { "enabled": true },
            "body_limit_bytes": 1_048_576,
            "token_cache": { "ttl_secs": 60, "capacity": 500 },
            "request_timeout_secs": 9
        });
        let cfg: OagwConfig = serde_json::from_value(json).expect("valid full config");
        assert_eq!(cfg.proxy_timeout_secs, 7);
        assert!(!cfg.allow_http_upstream);
        assert!(cfg.ssrf_policy.enabled, "ssrf policy block deserializes");
        assert_eq!(cfg.body_limit_bytes, 1_048_576);
        assert_eq!(cfg.token_cache.ttl_secs, 60);
        assert_eq!(cfg.token_cache.capacity, 500);
        assert_eq!(cfg.request_timeout_secs, 9);

        // Round-trip back to JSON keeps every field.
        let encoded = serde_json::to_value(&cfg).expect("serialize config");
        let decoded: OagwConfig = serde_json::from_value(encoded).expect("re-deserialize");
        assert_eq!(decoded.token_cache.ttl_secs, cfg.token_cache.ttl_secs);
        assert_eq!(decoded.body_limit_bytes, cfg.body_limit_bytes);
        assert_eq!(decoded.ssrf_policy.enabled, cfg.ssrf_policy.enabled);
    }

    #[test]
    fn defaults_are_reflected_on_disk_shape() {
        let cfg = OagwConfig::default();
        let value = serde_json::to_value(&cfg).expect("serialize defaults");
        // All fields must be present so ops can override without surprises.
        for field in [
            "proxy_timeout_secs",
            "allow_http_upstream",
            "ssrf_policy",
            "body_limit_bytes",
            "token_cache",
            "request_timeout_secs",
        ] {
            assert!(
                value.get(field).is_some(),
                "default config contains {field}"
            );
        }
    }
}
