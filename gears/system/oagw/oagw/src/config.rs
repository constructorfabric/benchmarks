//! Configuration for the OAGW gear.
//!
//! Loaded from the runtime config under `gears.oagw.config` (see
//! `config/e2e-local.yaml`). All fields carry sane defaults so the gear
//! boots without an explicit section.

use serde::Deserialize;

/// Default per-request proxy timeout in seconds.
pub const DEFAULT_PROXY_TIMEOUT_SECS: u64 = 30;

/// Default token cache TTL ceiling in seconds.
pub const DEFAULT_TOKEN_CACHE_TTL_SECS: u64 = 300;

/// Default token cache capacity.
pub const DEFAULT_TOKEN_CACHE_CAPACITY: usize = 10_000;

/// Configuration for the OAGW gear.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct OagwConfig {
    /// Per-request upstream proxy timeout in seconds. Applies to the
    /// end-to-end proxy operation (connection + request + response headers).
    pub proxy_timeout_secs: u64,

    /// Allow plaintext `http://` upstream endpoints. Intended for local
    /// development and e2e testing only; when `false` (default), an upstream
    /// declared with `scheme: http` is rejected at validation time.
    pub allow_http_upstream: bool,

    /// Server-Side Request Forgery policy for the data plane.
    #[serde(default)]
    pub ssrf_policy: SsrfPolicyConfig,

    /// Shared OAuth2 token cache tuning for the `oauth2_client_cred` auth
    /// plugins (see `ADR 0008`).
    #[serde(default)]
    pub token_cache: TokenCacheConfig,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: DEFAULT_PROXY_TIMEOUT_SECS,
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicyConfig::default(),
            token_cache: TokenCacheConfig::default(),
        }
    }
}

/// SSRF policy.
///
/// When `enabled`, the data plane refuses to connect to link-local /
/// loopback / private address space upstream targets. Disabled for e2e
/// testing where upstreams run on `localhost`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SsrfPolicyConfig {
    /// Whether SSRF protection is enabled.
    pub enabled: bool,
}

impl Default for SsrfPolicyConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Tuning for the process-wide OAuth2 access-token cache.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TokenCacheConfig {
    /// Ceiling for a cached token's TTL. The effective TTL is
    /// `min(ttl_secs, expires_in - 30s)`.
    pub cache_ttl_secs: u64,

    /// Maximum number of cached token entries before eviction.
    pub cache_capacity: usize,
}

impl Default for TokenCacheConfig {
    fn default() -> Self {
        Self {
            cache_ttl_secs: DEFAULT_TOKEN_CACHE_TTL_SECS,
            cache_capacity: DEFAULT_TOKEN_CACHE_CAPACITY,
        }
    }
}
