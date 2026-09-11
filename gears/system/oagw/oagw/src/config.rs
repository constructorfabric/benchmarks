//! Gear configuration, loaded from the `gears.oagw.config` section.

use serde::Deserialize;

/// Hard ceiling for a proxied request body (100 MiB).
const DEFAULT_MAX_BODY_BYTES: u64 = 100 * 1024 * 1024;
/// Default outbound proxy timeout.
const DEFAULT_PROXY_TIMEOUT_SECS: u64 = 30;
/// Default ceiling for a cached `OAuth2` access token.
const DEFAULT_TOKEN_CACHE_TTL_SECS: u64 = 300;
/// Default number of entries in the `OAuth2` token cache.
const DEFAULT_TOKEN_CACHE_CAPACITY: usize = 10_000;

/// Top-level OAGW gear configuration.
///
/// Mirrors the `gears.oagw.config` section of the server configuration file.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OagwConfig {
    /// Proxy request timeout in seconds. Applies to the whole upstream call.
    pub proxy_timeout_secs: u64,
    /// Whether plaintext (`http`) upstream endpoints may be configured.
    pub allow_http_upstream: bool,
    /// SSRF protection settings.
    pub ssrf_policy: SsrfPolicy,
    /// Maximum accepted request body size in bytes.
    pub max_request_body_bytes: u64,
    /// Ceiling for a cached `OAuth2` access token, in seconds.
    pub token_cache_ttl_secs: u64,
    /// Maximum entries held by the `OAuth2` token cache.
    pub token_cache_capacity: usize,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: DEFAULT_PROXY_TIMEOUT_SECS,
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicy::default(),
            max_request_body_bytes: DEFAULT_MAX_BODY_BYTES,
            token_cache_ttl_secs: DEFAULT_TOKEN_CACHE_TTL_SECS,
            token_cache_capacity: DEFAULT_TOKEN_CACHE_CAPACITY,
        }
    }
}

impl OagwConfig {
    /// Proxy timeout as a [`Duration`].
    #[must_use]
    pub fn proxy_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.proxy_timeout_secs.max(1))
    }

    /// Reject settings that would leave the proxy in an unusable state.
    ///
    /// # Errors
    /// Returns a description of the offending field.
    pub fn validate(&self) -> Result<(), String> {
        if self.proxy_timeout_secs == 0 {
            return Err("proxy_timeout_secs must be at least 1".to_owned());
        }
        if self.max_request_body_bytes == 0 {
            return Err("max_request_body_bytes must be greater than zero".to_owned());
        }
        if self.token_cache_capacity == 0 {
            return Err("token_cache_capacity must be greater than zero".to_owned());
        }
        Ok(())
    }
}

/// SSRF protection settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SsrfPolicy {
    /// Enable SSRF protection (blocking loopback / link-local upstreams).
    pub enabled: bool,
}

impl Default for SsrfPolicy {
    fn default() -> Self {
        Self { enabled: true }
    }
}
