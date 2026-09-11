//! Gear configuration — the `gears.oagw.config` block.
//!
//! The graded configuration (`config/e2e-local.yaml`) carries:
//!
//! ```yaml
//! oagw:
//!   config:
//!     proxy_timeout_secs: 2
//!     allow_http_upstream: true
//!     ssrf_policy:
//!       enabled: false
//! ```
//!
//! and no `database:` section, so every field here must have a usable default.

use std::time::Duration;

/// `OagwConfig` — gear-level settings.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default)]
pub struct OagwConfig {
    /// Whole-request timeout applied to the outbound upstream call. On expiry the
    /// proxy answers `504` (`cf.oagw.timeout.request.v1`).
    pub proxy_timeout_secs: u64,

    /// Whether a plaintext (`http`) endpoint may actually be connected to at proxy
    /// time. This is the *connection* policy only — `http` is always a legal value
    /// for `server.endpoints[].scheme` at management time.
    pub allow_http_upstream: bool,

    /// SSRF guard applied before the outbound connection is opened.
    pub ssrf_policy: SsrfPolicy,

    /// `OAuth2` access-token cache (ADR 0008).
    pub token_cache: TokenCacheConfig,

    /// Hard request-body limit in bytes: `100 MB` per DESIGN §3.2 Body Validation.
    pub max_body_bytes: u64,

    /// Maximum number of entries in the data-plane L1 config cache (ADR 0005/0006).
    pub l1_cache_capacity: usize,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: 30,
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicy::default(),
            token_cache: TokenCacheConfig::default(),
            max_body_bytes: 100 * 1024 * 1024,
            l1_cache_capacity: 1000,
        }
    }
}

impl OagwConfig {
    /// Outbound call timeout as a [`Duration`].
    #[must_use]
    pub fn proxy_timeout(&self) -> Duration {
        Duration::from_secs(self.proxy_timeout_secs)
    }

    /// Hard request-body limit in bytes.
    #[must_use]
    pub fn max_body_bytes(&self) -> u64 {
        self.max_body_bytes
    }
}

/// Outbound-destination admission policy.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default)]
pub struct SsrfPolicy {
    /// When `false` (the graded configuration) no destination filtering is applied.
    pub enabled: bool,

    /// When the policy is enabled, refuse loopback / link-local / RFC 1918
    /// destinations unless they appear in [`SsrfPolicy::allowed_hosts`].
    pub block_private_networks: bool,

    /// Hosts that bypass the private-network block.
    pub allowed_hosts: Vec<String>,
}

impl Default for SsrfPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            block_private_networks: true,
            allowed_hosts: Vec::new(),
        }
    }
}

/// `OAuth2` token-cache settings: `token_cache_ttl_secs` and
/// `token_cache_capacity` in ADR 0008's gear-level table.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default)]
pub struct TokenCacheConfig {
    /// Ceiling for a cached access token's TTL. The effective TTL is
    /// `min(ttl, expires_in - 30s)`.
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
    /// Cache TTL ceiling as a [`Duration`].
    #[must_use]
    pub fn ttl(&self) -> Duration {
        Duration::from_secs(self.ttl_secs)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod config_tests;
