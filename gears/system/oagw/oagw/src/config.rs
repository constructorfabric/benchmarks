//! Configuration for the OAGW (outbound API gateway) gear.
//!
//! Read from the `gears.oagw.config` YAML section via
//! [`GearCtx::config_or_default::<OagwConfig>()`](crate::toolkit::GearCtx::config_or_default).

use serde::Deserialize;

/// Maximum upstream request/response body size in bytes (100 MiB).
///
/// Per the DESIGN contract, bodies larger than this are rejected with a `413`
/// *before* buffering (when `Content-Length` is present and exceeds the limit)
/// or as soon as the limit is crossed while buffering.
pub const DEFAULT_BODY_LIMIT_BYTES: usize = 100 * 1024 * 1024;

/// Default proxy timeout in seconds (applied when config omits it).
pub const DEFAULT_PROXY_TIMEOUT_SECS: u64 = 30;

/// OAGW gear configuration.
///
/// Every field has a safe default; a gear may be started with no `config`
/// section at all ([`GearCtx::config_or_default`] falls back to `Default`).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct OagwConfig {
    /// Maximum time to wait for an upstream to respond (seconds).
    pub proxy_timeout_secs: u64,
    /// Whether plain-HTTP (`http://`) upstreams are allowed.
    ///
    /// `false` (default) fails closed: an `http` upstream is rejected with a
    /// gateway-side error. `true` is enabled only for testing.
    pub allow_http_upstream: bool,
    /// SSRF (server-side request forgery) protection policy.
    pub ssrf_policy: SsrfPolicy,
    /// Token-cache TTL in seconds for auth plugins (ADR 0008).
    pub token_cache_ttl_secs: u64,
    /// Token-cache capacity (number of entries) for auth plugins (ADR 0008).
    pub token_cache_capacity: usize,
    /// Maximum accepted upstream request body size in bytes.
    pub body_limit_bytes: usize,
    /// Circuit-breaker window override (kept here for config compatibility;
    /// the MVP uses a fixed in-memory breaker window of 60s).
    #[allow(dead_code)]
    pub circuit_breaker_window_secs: u64,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: DEFAULT_PROXY_TIMEOUT_SECS,
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicy::default(),
            // ADR 0008 defaults.
            token_cache_ttl_secs: 300,
            token_cache_capacity: 10_000,
            body_limit_bytes: DEFAULT_BODY_LIMIT_BYTES,
            circuit_breaker_window_secs: 60,
        }
    }
}

/// SSRF protection policy applied before any upstream is contacted.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SsrfPolicy {
    /// Whether SSRF protection is enabled (defaults to `true` — fail closed).
    pub enabled: bool,
    /// Comma-separated hostname/IP allowlist; empty means "no explicit allowlist".
    #[allow(dead_code)]
    pub allowlist: Vec<String>,
    /// Comma-separated hostname/IP denylist; empty means "no explicit denylist".
    #[allow(dead_code)]
    pub denylist: Vec<String>,
}

impl Default for SsrfPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            allowlist: Vec::new(),
            denylist: Vec::new(),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_safe() {
        let cfg = OagwConfig::default();
        // Fail-closed defaults.
        assert!(!cfg.allow_http_upstream);
        assert!(cfg.ssrf_policy.enabled);
        assert_eq!(cfg.body_limit_bytes, DEFAULT_BODY_LIMIT_BYTES);
        // ADR 0008 defaults.
        assert_eq!(cfg.token_cache_ttl_secs, 300);
        assert_eq!(cfg.token_cache_capacity, 10_000);
    }

    #[test]
    fn deserializes_e2e_config_block() {
        // Mirrors /app/config/e2e-local.yaml `gears.oagw.config` plus ADR 0008 keys.
        let json = r#"{
            "proxy_timeout_secs": 2,
            "allow_http_upstream": true,
            "ssrf_policy": { "enabled": false },
            "token_cache_ttl_secs": 300,
            "token_cache_capacity": 10000
        }"#;
        let cfg: OagwConfig =
            serde_json::from_str(json).expect("e2e config block must deserialize");
        assert_eq!(cfg.proxy_timeout_secs, 2);
        assert!(cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.enabled);
        assert_eq!(cfg.token_cache_ttl_secs, 300);
        // Fields absent from the block fall back to `serde(default)` defaults.
        assert_eq!(cfg.body_limit_bytes, DEFAULT_BODY_LIMIT_BYTES);
    }

    #[test]
    fn partial_config_falls_back_to_defaults() {
        let cfg: OagwConfig = serde_json::from_str(r#"{"proxy_timeout_secs": 5}"#).unwrap();
        assert_eq!(cfg.proxy_timeout_secs, 5);
        assert!(!cfg.allow_http_upstream);
        assert!(cfg.ssrf_policy.enabled);
    }
}
