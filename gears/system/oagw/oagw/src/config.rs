//! Typed configuration surface for the OAGW gear.
//!
//! Realizes `cpt-cf-oagw-algo-gf-load-config` / `cpt-cf-oagw-dod-gf-config`.
//!
//! The struct deliberately does **not** use `deny_unknown_fields`: the gear must
//! start when the deployment configuration carries a key this build does not
//! recognize.

use serde::Deserialize;

/// Server-Side Request Forgery policy for outbound connections.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SsrfPolicy {
    /// When true, resolved outbound targets are screened before connecting.
    pub enabled: bool,
}

impl Default for SsrfPolicy {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Gear-level configuration, deserialized from `gears.oagw.config`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct OagwConfig {
    /// Bound on establishing an upstream connection and completing a
    /// non-streaming exchange, in seconds.
    pub proxy_timeout_secs: u64,
    /// Whether a plaintext (non-TLS) upstream connection may actually be made.
    ///
    /// This governs the *connection*, never which scheme values the management
    /// API accepts at create time.
    pub allow_http_upstream: bool,
    /// SSRF screening policy.
    pub ssrf_policy: SsrfPolicy,
    /// Ceiling on how long a cached authorization token is retained, in seconds.
    pub token_cache_ttl_secs: u64,
    /// Maximum number of cached authorization tokens.
    pub token_cache_capacity: usize,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: 30,
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicy::default(),
            token_cache_ttl_secs: 300,
            token_cache_capacity: 10_000,
        }
    }
}

impl OagwConfig {
    /// The proxy timeout as a `Duration`.
    #[must_use]
    pub fn proxy_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.proxy_timeout_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_yields_documented_defaults() {
        let cfg: OagwConfig = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(cfg.proxy_timeout_secs, 30);
        assert!(!cfg.allow_http_upstream);
        assert!(cfg.ssrf_policy.enabled);
        assert_eq!(cfg.token_cache_ttl_secs, 300);
        assert_eq!(cfg.token_cache_capacity, 10_000);
    }

    #[test]
    fn graded_configuration_deserializes() {
        // Mirrors the `gears.oagw.config` block of config/e2e-local.yaml.
        let cfg: OagwConfig = serde_json::from_value(serde_json::json!({
            "proxy_timeout_secs": 2,
            "allow_http_upstream": true,
            "ssrf_policy": { "enabled": false }
        }))
        .unwrap();
        assert_eq!(cfg.proxy_timeout_secs, 2);
        assert!(cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.enabled);
        // Keys absent from the graded config keep their defaults.
        assert_eq!(cfg.token_cache_ttl_secs, 300);
        assert_eq!(cfg.token_cache_capacity, 10_000);
    }

    #[test]
    fn unknown_keys_do_not_break_startup() {
        let cfg: OagwConfig = serde_json::from_value(serde_json::json!({
            "proxy_timeout_secs": 7,
            "a_key_this_build_does_not_know": {"nested": [1, 2, 3]}
        }))
        .unwrap();
        assert_eq!(cfg.proxy_timeout_secs, 7);
    }

    #[test]
    fn proxy_timeout_is_derived_from_seconds() {
        let cfg = OagwConfig {
            proxy_timeout_secs: 2,
            ..OagwConfig::default()
        };
        assert_eq!(cfg.proxy_timeout(), std::time::Duration::from_secs(2));
    }
}
