//! Gear-level configuration.
//!
//! Read once in [`crate::gear::OagwGear::init`] via
//! `ctx.config_or_default::<OagwConfig>()`. The struct is deliberately
//! permissive about unknown keys — a deployment that carries an extra knob in
//! its `oagw.config` block must not prevent the gear from starting.

use serde::Deserialize;

/// Default ceiling on a proxied request body: 100 MiB.
pub const DEFAULT_MAX_BODY_SIZE_BYTES: usize = 100 * 1024 * 1024;
/// Default time the data plane waits for the upstream response.
pub const DEFAULT_PROXY_TIMEOUT_SECS: u64 = 30;
/// Default time the data plane waits for a TCP/TLS connection.
pub const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 10;
/// Default idle connections kept alive per upstream host.
pub const DEFAULT_CONNECTION_POOL_SIZE: usize = 128;

/// Outbound-request guard against server-side request forgery.
///
/// When `enabled`, endpoints whose host is an IP literal inside a private
/// range, or a hostname that resolves into one, are refused before any
/// connection is attempted — unless the host is listed in `allowed_hosts`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(default)]
#[derive(Default)]
pub struct SsrfPolicy {
    /// Whether the guard is active. Disabled by default.
    pub enabled: bool,
    /// Hosts explicitly exempt from the guard.
    pub allowed_hosts: Vec<String>,
    /// Allow loopback / link-local / private ranges even when enabled.
    pub allow_private_networks: bool,
}


/// Circuit-breaker policy applied per upstream host (core policy, not a plugin).
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct CircuitBreakerConfig {
    /// Failures inside `window_secs` that trip the breaker.
    pub failure_threshold: u32,
    /// Sliding window the failures are counted in, in seconds.
    pub window_secs: u64,
    /// How long an open breaker stays open before probing, in seconds.
    pub open_secs: u64,
    /// `false` disables the breaker entirely.
    pub enabled: bool,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            window_secs: 30,
            open_secs: 30,
            enabled: true,
        }
    }
}

/// Gear-level configuration for the outbound API gateway.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct OagwConfig {
    /// Ceiling on a proxied request body; larger requests are rejected with
    /// `413` before any buffering happens.
    pub max_body_size_bytes: usize,
    /// Time the data plane waits for the upstream to produce response headers.
    pub proxy_timeout_secs: u64,
    /// Time the data plane waits for the upstream TCP/TLS connection.
    pub connect_timeout_secs: u64,
    /// Accept plaintext (`http`/`ws`) upstream endpoints.
    pub allow_http_upstream: bool,
    /// Outbound SSRF guard.
    pub ssrf_policy: SsrfPolicy,
    /// OAuth2 token cache TTL, in seconds.
    pub token_cache_ttl_secs: u64,
    /// OAuth2 token cache capacity, in entries.
    pub token_cache_capacity: usize,
    /// Idle connections kept alive per upstream host.
    pub connection_pool_size: usize,
    /// Per-upstream circuit breaker.
    pub circuit_breaker: CircuitBreakerConfig,
    /// Upper bound on the number of distinct rate-limit buckets the gear keeps.
    pub rate_limit_buckets: usize,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            max_body_size_bytes: DEFAULT_MAX_BODY_SIZE_BYTES,
            proxy_timeout_secs: DEFAULT_PROXY_TIMEOUT_SECS,
            connect_timeout_secs: DEFAULT_CONNECT_TIMEOUT_SECS,
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicy::default(),
            token_cache_ttl_secs: 300,
            token_cache_capacity: 10_000,
            connection_pool_size: DEFAULT_CONNECTION_POOL_SIZE,
            circuit_breaker: CircuitBreakerConfig::default(),
            rate_limit_buckets: 65_536,
        }
    }
}

impl OagwConfig {
    /// Proxy request timeout as a [`std::time::Duration`].
    #[must_use]
    pub fn proxy_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.proxy_timeout_secs.max(1))
    }

    /// Upstream connect timeout as a [`std::time::Duration`].
    #[must_use]
    pub fn connect_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.connect_timeout_secs.max(1))
    }

    /// OAuth2 token cache TTL as a [`std::time::Duration`].
    #[must_use]
    pub fn token_cache_ttl(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.token_cache_ttl_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_documented_values() {
        let cfg = OagwConfig::default();
        assert_eq!(cfg.max_body_size_bytes, 100 * 1024 * 1024);
        assert_eq!(cfg.proxy_timeout_secs, 30);
        assert_eq!(cfg.token_cache_ttl_secs, 300);
        assert_eq!(cfg.token_cache_capacity, 10_000);
        assert!(!cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.enabled);
        assert!(cfg.circuit_breaker.enabled);
        assert_eq!(cfg.circuit_breaker.failure_threshold, 5);
    }

    fn from_json(value: serde_json::Value) -> OagwConfig {
        serde_json::from_value(value).expect("valid config document")
    }

    #[test]
    fn parses_the_e2e_config_shape() {
        let cfg = from_json(serde_json::json!({
            "proxy_timeout_secs": 2,
            "allow_http_upstream": true,
            "ssrf_policy": { "enabled": false },
        }));
        assert_eq!(cfg.proxy_timeout_secs, 2);
        assert!(cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.enabled);
        // Unspecified fields fall back to their defaults.
        assert_eq!(cfg.max_body_size_bytes, DEFAULT_MAX_BODY_SIZE_BYTES);
    }

    #[test]
    fn ignores_unknown_keys() {
        let cfg = from_json(serde_json::json!({
            "proxy_timeout_secs": 5,
            "unknown_future_field": 7,
        }));
        assert_eq!(cfg.proxy_timeout_secs, 5);
    }

    #[test]
    fn empty_document_yields_defaults() {
        assert_eq!(from_json(serde_json::json!({})), OagwConfig::default());
    }
}
