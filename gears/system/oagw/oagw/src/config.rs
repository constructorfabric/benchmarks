//! Gear-level configuration (`gears.oagw.config` in the deployment config).

use std::time::Duration;

use serde::Deserialize;

/// SSRF guard posture for upstream endpoint resolution.
///
/// The e2e deployment runs with `enabled: false` (upstream endpoints may point
/// at loopback/private addresses); when enabled, private and loopback targets
/// are rejected before the connection is opened.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct SsrfPolicy {
    pub enabled: bool,
    /// Also reject link-local and unique-local addresses, not just RFC1918/loopback.
    pub block_link_local: bool,
}

impl Default for SsrfPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            block_link_local: true,
        }
    }
}

/// OAuth2 client-credentials token cache settings (ADR-0008).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct TokenCacheConfig {
    /// Ceiling for the cached access token TTL, in seconds (see ADR-0008).
    pub ttl_secs: u64,
    /// Maximum number of cached tokens.
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
    /// Configured TTL ceiling.
    pub fn ttl(&self) -> Duration {
        Duration::from_secs(self.ttl_secs.max(1))
    }
}

/// Gear-level configuration (`gears.oagw.config`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct OagwConfig {
    /// Maximum time to wait for upstream response headers (seconds).
    pub proxy_timeout_secs: u64,
    /// Allow plaintext `http` upstream endpoints. FIPS builds keep this off.
    pub allow_http_upstream: bool,
    /// Reject upstream endpoints pointing at private/loopback addresses.
    pub ssrf_policy: SsrfPolicy,
    /// Hard request-body ceiling in bytes (DESIGN: 100 MB → 413).
    pub max_body_bytes: usize,
    /// L1 (data plane) configuration cache TTL in seconds.
    pub l1_cache_ttl_secs: u64,
    /// Token cache for the OAuth2 client-credentials plugin.
    pub token_cache: TokenCacheConfig,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: 30,
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicy::default(),
            max_body_bytes: 100 * 1024 * 1024,
            l1_cache_ttl_secs: 5,
            token_cache: TokenCacheConfig::default(),
        }
    }
}

impl OagwConfig {
    /// Upstream response-header timeout.
    pub fn proxy_timeout(&self) -> Duration {
        Duration::from_secs(self.proxy_timeout_secs.max(1))
    }
}
