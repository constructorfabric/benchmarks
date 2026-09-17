//! OAGW gear configuration.
//!
//! Sourced from `oagw.config` in the server configuration file
//! (see `config/e2e-local.yaml`).

use serde::Deserialize;
use std::time::Duration;

/// Server-Side Request Forgery policy knobs.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case", default)]
pub struct SsrfPolicy {
    /// Enable DNS / IP-range pinning checks before dialling an upstream.
    pub enabled: bool,
    /// Additional CIDR ranges (IPv4/IPv6) that must never be dialled.
    pub blocked_ranges: Vec<String>,
}

impl Default for SsrfPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            blocked_ranges: Vec::new(),
        }
    }
}

/// Gear-level OAGW configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case", default)]
pub struct OagwConfig {
    /// Total time allowed for establishing the upstream connection.
    pub connect_timeout_secs: u64,
    /// Total time allowed for a non-streaming upstream request/response.
    pub proxy_timeout_secs: u64,
    /// Idle timeout applied while no bytes flow on a streaming response.
    pub idle_timeout_secs: u64,
    /// Hard cap on buffered request bodies.
    pub max_body_bytes: usize,
    /// Allow plaintext `http://` endpoints. Disabled by default — see
    /// `cpt-cf-oagw-constraint-https-only`.
    pub allow_http_upstream: bool,
    /// Allow self-signed / unverified upstream TLS certificates.
    pub allow_insecure_tls: bool,
    /// SSRF guard configuration.
    pub ssrf_policy: SsrfPolicy,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            connect_timeout_secs: 10,
            proxy_timeout_secs: 60,
            idle_timeout_secs: 300,
            max_body_bytes: 100 * 1024 * 1024,
            allow_http_upstream: false,
            allow_insecure_tls: false,
            ssrf_policy: SsrfPolicy::default(),
        }
    }
}

impl OagwConfig {
    /// Connect timeout as a [`Duration`].
    #[must_use]
    pub fn connect_timeout(&self) -> Duration {
        Duration::from_secs(self.connect_timeout_secs.max(1))
    }

    /// Overall proxy timeout as a [`Duration`].
    #[must_use]
    pub fn proxy_timeout(&self) -> Duration {
        Duration::from_secs(self.proxy_timeout_secs.max(1))
    }

    /// Streaming idle timeout as a [`Duration`].
    #[must_use]
    pub fn idle_timeout(&self) -> Duration {
        Duration::from_secs(self.idle_timeout_secs.max(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_https_only() {
        let cfg = OagwConfig::default();
        assert!(!cfg.allow_http_upstream);
        assert_eq!(cfg.max_body_bytes, 100 * 1024 * 1024);
    }

    #[test]
    fn deserializes_e2e_shape() {
        let cfg: OagwConfig = serde_json::from_value(serde_json::json!({
            "proxy_timeout_secs": 2,
            "allow_http_upstream": true,
            "ssrf_policy": { "enabled": false }
        }))
        .expect("valid config");
        assert!(cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.enabled);
        assert_eq!(cfg.proxy_timeout(), Duration::from_secs(2));
    }
}
