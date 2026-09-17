//! Runtime configuration for the OAGW gear.
//!
//! Sourced from the `oagw.config` section of the server configuration
//! (`config_expanded_or_default`), with documented defaults for every field so
//! a missing section still yields a working gear.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Default ceiling for a cached `OAuth2` access token (5 minutes).
pub const DEFAULT_TOKEN_CACHE_TTL_SECS: u64 = 300;
/// Default maximum number of entries in the `OAuth2` token cache.
pub const DEFAULT_TOKEN_CACHE_CAPACITY: usize = 10_000;
/// Default upstream request/response deadline, in seconds.
pub const DEFAULT_PROXY_TIMEOUT_SECS: u64 = 30;
/// Hard limit on a proxied request body (PRD: reject before buffering).
pub const MAX_REQUEST_BODY_BYTES: usize = 100 * 1024 * 1024;
/// `Access-Control-Max-Age` sent on preflight responses.
pub const CORS_PREFLIGHT_MAX_AGE: u64 = 86_400;

/// Server-side request-forgery policy applied before an upstream is dialed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SsrfPolicy {
    /// Enable outbound host validation. Disabled by default so a deployment
    /// that has not been given an explicit allow/deny list keeps working.
    pub enabled: bool,
    /// Deny endpoint hosts that resolve into link-local / loopback /
    /// private address space (only meaningful when `enabled`).
    pub deny_private_addresses: bool,
    /// Explicit allowlist of endpoint hostnames/IPs. Empty means no allowlist.
    pub allowed_hosts: Vec<String>,
}

impl Default for SsrfPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            deny_private_addresses: true,
            allowed_hosts: Vec::new(),
        }
    }
}

/// OAGW gear configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OagwConfig {
    /// Deadline applied to a single upstream exchange (headers + buffered
    /// body). Streaming responses are bounded per-read by the same value.
    pub proxy_timeout_secs: u64,
    /// Permit plaintext (`http`/`ws`) upstream endpoints. When `false`,
    /// dialing a plaintext endpoint fails with `link.unavailable` before any
    /// bytes are written to the wire.
    pub allow_http_upstream: bool,
    /// Outbound SSRF guard configuration.
    pub ssrf_policy: SsrfPolicy,
    /// Ceiling for a cached `OAuth2` access-token TTL (seconds).
    pub token_cache_ttl_secs: u64,
    /// Maximum entries in the `OAuth2` token cache.
    pub token_cache_capacity: usize,
    /// Hard limit on the buffered request body forwarded to an upstream.
    pub max_request_body_bytes: usize,
    /// Whether `X-RateLimit-*` headers are emitted when a rate limit is
    /// configured for the resolved route/upstream.
    pub rate_limit_response_headers: bool,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: DEFAULT_PROXY_TIMEOUT_SECS,
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicy::default(),
            token_cache_ttl_secs: DEFAULT_TOKEN_CACHE_TTL_SECS,
            token_cache_capacity: DEFAULT_TOKEN_CACHE_CAPACITY,
            max_request_body_bytes: MAX_REQUEST_BODY_BYTES,
            rate_limit_response_headers: true,
        }
    }
}

impl OagwConfig {
    /// Upstream deadline as a `Duration`.
    #[must_use]
    pub fn proxy_timeout(&self) -> Duration {
        Duration::from_secs(self.proxy_timeout_secs.max(1))
    }

    /// Ceiling for a cached `OAuth2` access token.
    #[must_use]
    pub fn token_cache_ttl(&self) -> Duration {
        Duration::from_secs(self.token_cache_ttl_secs.max(1))
    }
}
