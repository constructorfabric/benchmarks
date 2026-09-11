//! Gear-level configuration for the Outbound API Gateway.
//!
//! Mirrors the `gears.oagw.config` block of the server configuration. Every
//! field is optional; the defaults are the production posture described by
//! `DESIGN.md` (HTTPS-only upstreams, SSRF guard on).

use std::time::Duration;

use serde::Deserialize;

/// Hard body limit from `cpt-cf-oagw-constraint-body-limit` (100 MB).
pub const DEFAULT_MAX_BODY_BYTES: u64 = 100 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct OagwConfig {
    /// Wall-clock budget for a single proxied request (connect + headers +
    /// body). Exceeding it yields `504 RequestTimeout`.
    pub proxy_timeout_secs: u64,
    /// TCP/TLS connect budget. Exceeding it yields `504 ConnectionTimeout`.
    pub connect_timeout_secs: u64,
    /// Lift `cpt-cf-oagw-constraint-https-only` for the *connection*: when
    /// false a plaintext endpoint is refused at request time. It never
    /// governs which schemes the management API accepts.
    pub allow_http_upstream: bool,
    /// Server-Side Request Forgery guard applied to resolved upstream IPs.
    pub ssrf_policy: SsrfPolicyConfig,
    /// Ceiling for cached OAuth2 access tokens (ADR-0008).
    pub token_cache_ttl_secs: u64,
    /// Maximum entries in the OAuth2 token cache (ADR-0008).
    pub token_cache_capacity: usize,
    /// Maximum inbound proxy body size, in bytes.
    pub max_body_bytes: u64,
    /// Data-plane L1 config cache capacity (ADR-0006).
    pub config_cache_capacity: usize,
    /// TTL after which an unlinked custom plugin becomes GC-eligible.
    pub plugin_gc_ttl_days: u64,
    /// Interval between plugin garbage-collection sweeps.
    pub plugin_gc_interval_secs: u64,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: 30,
            connect_timeout_secs: 10,
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicyConfig::default(),
            token_cache_ttl_secs: 300,
            token_cache_capacity: 10_000,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            config_cache_capacity: 1_000,
            plugin_gc_ttl_days: 30,
            plugin_gc_interval_secs: 3600,
        }
    }
}

impl OagwConfig {
    #[must_use]
    pub fn proxy_timeout(&self) -> Duration {
        Duration::from_secs(self.proxy_timeout_secs.max(1))
    }

    #[must_use]
    pub fn connect_timeout(&self) -> Duration {
        Duration::from_secs(self.connect_timeout_secs.max(1))
    }

    #[must_use]
    pub fn token_cache_ttl(&self) -> Duration {
        Duration::from_secs(self.token_cache_ttl_secs.max(1))
    }

    /// # Errors
    /// Returns a human-readable description when a field is out of range.
    pub fn validate(&self) -> Result<(), String> {
        if self.proxy_timeout_secs == 0 {
            return Err("proxy_timeout_secs must be > 0".to_owned());
        }
        if self.connect_timeout_secs == 0 {
            return Err("connect_timeout_secs must be > 0".to_owned());
        }
        if self.max_body_bytes == 0 {
            return Err("max_body_bytes must be > 0".to_owned());
        }
        if self.token_cache_capacity == 0 {
            return Err("token_cache_capacity must be > 0".to_owned());
        }
        Ok(())
    }
}

/// SSRF posture (`cpt-cf-oagw-nfr-ssrf-protection`).
///
/// When enabled, every resolved upstream address is checked against the
/// blocked address classes before a connection is opened.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SsrfPolicyConfig {
    pub enabled: bool,
    /// Permit loopback destinations (127.0.0.0/8, ::1) while the guard is on.
    pub allow_loopback: bool,
    /// Permit RFC 1918 / ULA destinations while the guard is on.
    pub allow_private_networks: bool,
    /// Hostnames that bypass the guard entirely (exact, ASCII-lowercased).
    pub allowed_hosts: Vec<String>,
}

impl Default for SsrfPolicyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allow_loopback: false,
            allow_private_networks: false,
            allowed_hosts: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::OagwConfig;

    #[test]
    fn defaults_are_production_posture() {
        let cfg = OagwConfig::default();
        assert!(!cfg.allow_http_upstream);
        assert!(cfg.ssrf_policy.enabled);
        assert_eq!(cfg.max_body_bytes, super::DEFAULT_MAX_BODY_BYTES);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn deserializes_the_e2e_shape() {
        let cfg: OagwConfig = serde_json::from_str(
            r#"{"proxy_timeout_secs":2,"allow_http_upstream":true,"ssrf_policy":{"enabled":false}}"#,
        )
        .expect("deserialize");
        assert_eq!(cfg.proxy_timeout_secs, 2);
        assert!(cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.enabled);
        // Unspecified fields keep their defaults.
        assert_eq!(cfg.token_cache_ttl_secs, 300);
    }

    #[test]
    fn unknown_keys_are_tolerated() {
        let cfg: OagwConfig =
            serde_json::from_str(r#"{"future_knob":true}"#).expect("deserialize");
        assert_eq!(cfg.proxy_timeout_secs, 30);
    }
}
