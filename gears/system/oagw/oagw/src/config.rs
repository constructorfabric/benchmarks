//! OAGW gear configuration (`gears.oagw.config`).
//!
//! The section is loaded with [`toolkit::context::GearCtx::config_or_default`]
//! (lenient: a deployment without an `oagw` section gets the documented
//! defaults) and rejects unknown keys so a typo fails loudly at startup.

use std::time::Duration;

use serde::{Deserialize, Serialize};

fn default_proxy_timeout_secs() -> u64 {
    30
}

fn default_body_limit_bytes() -> usize {
    100 * 1024 * 1024
}

fn default_token_cache_ttl_secs() -> u64 {
    300
}

fn default_token_cache_capacity() -> usize {
    10_000
}

fn default_ssrf_enabled() -> bool {
    true
}

/// Data-plane proxy timeout configuration knobs that are not per-resource.
///
/// Defaults: `proxy_timeout_secs = 30`, `allow_http_upstream = false`,
/// `ssrf_policy.enabled = true`, `token_cache = { ttl_secs = 300,
/// capacity = 10_000 }`, `body_limit_bytes = 100 MiB`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OagwConfig {
    /// Per-request upstream deadline in seconds (data plane).
    #[serde(default = "default_proxy_timeout_secs")]
    pub proxy_timeout_secs: u64,
    /// Allow dialing a plaintext (`http`/`ws`) upstream. `http` remains a legal
    /// create-time `scheme` either way: this flag only governs whether the data
    /// plane is allowed to actually open a plaintext connection.
    #[serde(default)]
    pub allow_http_upstream: bool,
    /// Server-side request forgery guard (DNS pinning / segment allowlisting).
    #[serde(default)]
    pub ssrf_policy: SsrfPolicy,
    /// In-process cache for auth-plugin tokens (ADR-0008).
    #[serde(default)]
    pub token_cache: TokenCacheConfig,
    /// Hard request body limit in bytes (100 MiB by default).
    #[serde(default = "default_body_limit_bytes")]
    pub body_limit_bytes: usize,
}

impl Default for OagwConfig {
    // Manual (not derived) so the defaults match the `#[serde(default = ...)]`
    // functions above; a derived `Default` would zero every field (a 0 s proxy
    // timeout or a 0 byte body limit would break the data plane).
    fn default() -> Self {
        Self {
            proxy_timeout_secs: default_proxy_timeout_secs(),
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicy::default(),
            token_cache: TokenCacheConfig::default(),
            body_limit_bytes: default_body_limit_bytes(),
        }
    }
}

impl OagwConfig {
    /// Upstream deadline as a [`Duration`].
    #[must_use]
    pub fn proxy_timeout(&self) -> Duration {
        Duration::from_secs(self.proxy_timeout_secs)
    }

    /// Whether a plaintext upstream connection may be dialed.
    #[must_use]
    pub const fn allows_http_upstream(&self) -> bool {
        self.allow_http_upstream
    }
}

/// Server-side request forgery policy for upstream target resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SsrfPolicy {
    /// Enforce IP pinning and private-segment allowlisting when resolving an
    /// upstream host. Enabled by default; the E2E configuration disables it so
    /// a loopback mock server can be used as an upstream.
    #[serde(default = "default_ssrf_enabled")]
    pub enabled: bool,
}

impl Default for SsrfPolicy {
    fn default() -> Self {
        Self {
            enabled: default_ssrf_enabled(),
        }
    }
}

/// In-process token cache used by the auth plugins (ADR-0008).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenCacheConfig {
    /// Cached token time-to-live in seconds.
    #[serde(default = "default_token_cache_ttl_secs")]
    pub ttl_secs: u64,
    /// Maximum number of cached tokens (LRU eviction beyond it).
    #[serde(default = "default_token_cache_capacity")]
    pub capacity: usize,
}

impl Default for TokenCacheConfig {
    fn default() -> Self {
        Self {
            ttl_secs: default_token_cache_ttl_secs(),
            capacity: default_token_cache_capacity(),
        }
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
