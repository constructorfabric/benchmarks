//! Gear configuration for the OAGW data plane.

use serde::{Deserialize, Serialize};

/// Server-Side Request Forgery protection policy.
///
/// Three boolean switches are required by the wire config contract
/// (`ssrf_policy: { enabled, block_loopback, block_private }`), so the struct
/// deliberately keeps them as booleans.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)]
pub struct SsrfConfig {
    /// Master switch; when `false` no SSRF checks run (e2e mode).
    pub enabled: bool,
    /// Whether loopback (127.0.0.0/8, `::1`) targets are blocked.
    pub block_loopback: bool,
    /// Whether RFC 1918 / link-local private ranges are blocked.
    pub block_private: bool,
}

impl Default for SsrfConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            block_loopback: true,
            block_private: true,
        }
    }
}

/// Token cache sizing for the `OAuth2` client-credentials plugin.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TokenCacheConfig {
    /// Ceiling (seconds) for a cached access token's TTL.
    pub ttl_secs: u64,
    /// Maximum number of cached token entries.
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

/// Top-level configuration for the electrical gateway gear.
///
/// The runtime places this under `gears.oagw.config` (see the example
/// `config/e2e-local.yaml`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)]
pub struct OagwConfig {
    /// Total request budget for one proxied call, in seconds. When exceeded
    /// the gateway returns `504 RequestTimeout`.
    pub proxy_timeout_secs: u64,
    /// Allow `http://` upstream targets (e2e). Production defaults to HTTPS.
    pub allow_http_upstream: bool,
    /// SSRF policy.
    pub ssrf_policy: SsrfConfig,
    /// `OAuth2` token cache sizing.
    pub token_cache: TokenCacheConfig,
    /// Hard request-body limit in bytes (default 100 MiB).
    pub max_body_size_bytes: usize,
    /// Whether the first entry of an inbound `X-Forwarded-For` header is
    /// trusted as the client address (used for IP-scoped rate limiting and
    /// diagnostics). Off by default: when false, the socket peer address is
    /// used and inbound proxy headers are stripped on the outbound request.
    pub trust_x_forwarded_for: bool,
    /// Allow `OAuth2` plugins to call plain-`http` token/issuer endpoints
    /// (e2e only). Off by default: non-`https` endpoints are rejected.
    pub allow_insecure_token_endpoint: bool,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: 30,
            allow_http_upstream: false,
            ssrf_policy: SsrfConfig::default(),
            token_cache: TokenCacheConfig::default(),
            max_body_size_bytes: 100 * 1024 * 1024,
            trust_x_forwarded_for: false,
            allow_insecure_token_endpoint: false,
        }
    }
}

impl OagwConfig {
    /// Validate the configuration invariants.
    ///
    /// # Errors
    ///
    /// Returns `Err(message)` when a value is out of range.
    pub fn validate(&self) -> Result<(), String> {
        if self.proxy_timeout_secs == 0 {
            return Err("proxy_timeout_secs must be > 0".to_owned());
        }
        if self.max_body_size_bytes == 0 {
            return Err("max_body_size_bytes must be > 0".to_owned());
        }
        if self.token_cache.capacity == 0 {
            return Err("token_cache.capacity must be > 0".to_owned());
        }
        Ok(())
    }
}
