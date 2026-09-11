//! Gear configuration (`gears.oagw.config` in the server YAML).

use serde::Deserialize;

/// SSRF policy knobs.
///
/// `ssrf_policy.enabled` guards against upstream declarations that point at
/// link-local / loopback / cloud-metadata addresses. It is **off** in the E2E
/// configuration because the acceptance suite proxies to loopback services.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub struct SsrfPolicy {
    #[serde(default)]
    pub enabled: bool,
}

/// `oagw` gear configuration.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct OagwConfig {
    /// Idle timeout applied to an upstream exchange (connection establishment,
    /// time to first response byte, and time between streamed body chunks).
    #[serde(default = "default_proxy_timeout_secs")]
    pub proxy_timeout_secs: u64,
    /// Whether `http` (plaintext) is an accepted upstream endpoint scheme.
    /// Accepting the scheme is a schema-level question; whether a plaintext
    /// connection is actually made is a deployment policy question governed by
    /// this flag's sibling controls (see `SsrfPolicy`).
    #[serde(default)]
    pub allow_http_upstream: bool,
    /// Security policy for outbound connections.
    #[serde(default)]
    pub ssrf_policy: SsrfPolicy,
    /// Ceiling for the OAuth2 client-credentials token cache entry TTL.
    #[serde(default = "default_token_cache_ttl_secs")]
    pub token_cache_ttl_secs: u64,
    /// Maximum number of entries in the OAuth2 client-credentials token cache.
    #[serde(default = "default_token_cache_capacity")]
    pub token_cache_capacity: usize,
    /// Maximum buffered request body size forwarded to an upstream.
    #[serde(default = "default_max_request_body_bytes")]
    pub max_request_body_bytes: usize,
    /// Circuit breaker: consecutive/rolling failures before a host is opened.
    #[serde(default = "default_circuit_breaker_failure_threshold")]
    pub circuit_breaker_failure_threshold: u32,
    /// Circuit breaker: how long an opened host stays open.
    #[serde(default = "default_circuit_breaker_window_secs")]
    pub circuit_breaker_window_secs: u64,
}

fn default_proxy_timeout_secs() -> u64 {
    30
}

fn default_token_cache_ttl_secs() -> u64 {
    300
}

fn default_token_cache_capacity() -> usize {
    10_000
}

fn default_max_request_body_bytes() -> usize {
    100 * 1024 * 1024
}

fn default_circuit_breaker_failure_threshold() -> u32 {
    5
}

fn default_circuit_breaker_window_secs() -> u64 {
    30
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: 30,
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicy::default(),
            token_cache_ttl_secs: 300,
            token_cache_capacity: 10_000,
            max_request_body_bytes: 100 * 1024 * 1024,
            circuit_breaker_failure_threshold: 5,
            circuit_breaker_window_secs: 30,
        }
    }
}

impl OagwConfig {
    /// Proxy timeout as a `Duration`.
    #[must_use]
    pub fn proxy_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.proxy_timeout_secs.max(1))
    }

    /// Circuit breaker open window.
    #[must_use]
    pub fn circuit_breaker_window(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.circuit_breaker_window_secs)
    }

    /// OAuth2 token cache ceiling.
    #[must_use]
    pub fn token_cache_ttl(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.token_cache_ttl_secs)
    }
}
