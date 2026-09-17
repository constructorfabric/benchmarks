//! OAGW gear configuration.
//!
//! Mounted in the example server under `gears.oagw.config`:
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
//! Every field has a safe default so the gear boots with no config present.

use serde::Deserialize;

/// Hard limit for buffered request payloads (RFC 9457 `413 PayloadTooLarge`).
pub const DEFAULT_BODY_LIMIT_BYTES: u64 = 100 * 1024 * 1024; // 100 MB

/// Default outbound proxy timeout (request/response) in seconds.
pub const DEFAULT_PROXY_TIMEOUT_SECS: u64 = 30;

/// Default OAuth2 token-cache TTL (ADR 0008).
pub const DEFAULT_TOKEN_CACHE_TTL_SECS: u64 = 300;

/// Default OAuth2 token-cache capacity (ADR 0008).
pub const DEFAULT_TOKEN_CACHE_CAPACITY: usize = 10_000;

/// Bundled OAuth2 token-cache settings (ADR 0008 §"Gear-Level Configuration").
#[derive(Debug, Clone, Copy)]
pub struct TokenCacheConfig {
    ttl: std::time::Duration,
    capacity: usize,
}

impl Default for TokenCacheConfig {
    fn default() -> Self {
        Self {
            ttl: std::time::Duration::from_secs(DEFAULT_TOKEN_CACHE_TTL_SECS),
            capacity: DEFAULT_TOKEN_CACHE_CAPACITY,
        }
    }
}

impl TokenCacheConfig {
    /// Ceiling for cached access-token TTL.
    #[must_use]
    pub fn ttl(&self) -> std::time::Duration {
        self.ttl
    }

    /// Maximum number of cached tokens.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

/// Outbound API gateway configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OagwConfig {
    /// Total timeout for a single proxied request (connect + response), in
    /// seconds. Exceeding it yields a `504 RequestTimeout`.
    pub proxy_timeout_secs: u64,

    /// When `false`, `http://` upstream endpoints are rejected with `400`
    /// (plaintext not allowed). `https://` is always permitted.
    pub allow_http_upstream: bool,

    /// Server-Side Request Forgery policy. When enabled, connections to
    /// private / reserved / link-local address ranges (and loopback
    /// hostnames) are rejected before any socket is opened.
    pub ssrf_policy: SsrfPolicyConfig,

    /// TTL (seconds) for cached OAuth2 access tokens (min of
    /// `expires_in - 30s` and this value).
    pub token_cache_ttl_secs: u64,

    /// Maximum number of cached OAuth2 access tokens per process.
    pub token_cache_capacity: usize,

    /// Maximum sized request body accepted by the proxy endpoint before
    /// buffering, in bytes. Exceeding it yields `413 PayloadTooLarge`.
    pub body_limit_bytes: u64,
}

/// SSRF protection policy.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SsrfPolicyConfig {
    /// When `true`, block connections to private/reserved IP ranges and
    /// non-public hostnames before connecting.
    pub enabled: bool,
}

impl Default for SsrfPolicyConfig {
    fn default() -> Self {
        Self { enabled: false }
    }
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: DEFAULT_PROXY_TIMEOUT_SECS,
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicyConfig::default(),
            token_cache_ttl_secs: DEFAULT_TOKEN_CACHE_TTL_SECS,
            token_cache_capacity: DEFAULT_TOKEN_CACHE_CAPACITY,
            body_limit_bytes: DEFAULT_BODY_LIMIT_BYTES,
        }
    }
}

impl OagwConfig {
    /// Validate config invariants.
    ///
    /// # Errors
    ///
    /// Returns an error message when a field is out of range.
    pub fn validate(&self) -> Result<(), String> {
        if self.proxy_timeout_secs == 0 {
            return Err("oagw.proxy_timeout_secs must be >= 1".to_owned());
        }
        if self.token_cache_ttl_secs == 0 {
            return Err("oagw.token_cache_ttl_secs must be >= 1".to_owned());
        }
        if self.token_cache_capacity == 0 {
            return Err("oagw.token_cache_capacity must be >= 1".to_owned());
        }
        if self.body_limit_bytes == 0 {
            return Err("oagw.body_limit_bytes must be >= 1".to_owned());
        }
        Ok(())
    }

    /// Timeout as a [`std::time::Duration`].
    #[must_use]
    pub fn proxy_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.proxy_timeout_secs)
    }

    /// Bundled OAuth2 token-cache settings.
    #[must_use]
    pub fn token_cache(&self) -> TokenCacheConfig {
        TokenCacheConfig {
            ttl: std::time::Duration::from_secs(self.token_cache_ttl_secs),
            capacity: self.token_cache_capacity,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_contract_docs() {
        let cfg = OagwConfig::default();
        assert_eq!(cfg.proxy_timeout_secs, 30);
        assert!(!cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.enabled);
        assert_eq!(cfg.token_cache_ttl_secs, 300);
        assert_eq!(cfg.token_cache_capacity, 10_000);
        assert_eq!(cfg.body_limit_bytes, 100 * 1024 * 1024);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn parses_the_e2e_mounted_shape() {
        // Mirrors config/e2e-local.yaml `oagw.config`.
        let cfg: OagwConfig = serde_json::from_value(serde_json::json!({
            "proxy_timeout_secs": 2,
            "allow_http_upstream": true,
            "ssrf_policy": { "enabled": false },
        }))
        .unwrap();
        assert_eq!(cfg.proxy_timeout_secs, 2);
        assert!(cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.enabled);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let res: Result<OagwConfig, _> = serde_json::from_value(serde_json::json!({
            "proxy_timeout_secs": 2,
            "not_a_field": true,
        }));
        assert!(res.is_err());
    }
    #[test]
    fn zero_timeout_is_invalid() {
        let cfg = OagwConfig {
            proxy_timeout_secs: 0,
            ..OagwConfig::default()
        };
        assert!(cfg.validate().is_err());
    }
}
