//! Gear-level configuration for the outbound API gateway.
//!
//! Everything here is optional: the gear boots with secure defaults when the
//! deployment supplies no `gears.oagw.config` block at all. Only the fields an
//! operator actually needs to relax (plaintext upstreams, SSRF policy in a
//! sandboxed test environment) have to be spelled out.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Hard ceiling on a proxied request body, per
/// `cpt-cf-oagw-constraint-body-limit` (100 MB).
pub const BODY_LIMIT_BYTES: usize = 100 * 1024 * 1024;

/// SSRF guard policy applied to resolved upstream addresses.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct SsrfPolicy {
    /// Whether the guard is active. Enabled by default
    /// (`cpt-cf-oagw-nfr-ssrf-protection`); deployments that legitimately
    /// proxy to in-cluster or loopback addresses turn it off explicitly.
    pub enabled: bool,
    /// When the guard is enabled, allow loopback / private / link-local
    /// destinations anyway. Off by default.
    pub allow_private_networks: bool,
}

impl Default for SsrfPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            allow_private_networks: false,
        }
    }
}

/// Token-cache knobs for the OAuth2 client-credentials auth plugin
/// (`cpt-cf-oagw-adr-oauth2-client-credentials-auth-plugin`).
#[derive(Debug, Clone, Copy)]
pub struct TokenCacheConfig {
    /// Ceiling for a cached access token's TTL.
    pub ttl: Duration,
    /// Maximum number of cached tokens.
    pub capacity: usize,
}

/// `OagwConfig` — the `gears.oagw.config` block.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct OagwConfig {
    /// Overall deadline for a single proxied request/response exchange.
    pub proxy_timeout_secs: u64,
    /// Deadline for establishing the upstream connection.
    pub connect_timeout_secs: u64,
    /// Idle read deadline while streaming an upstream response (SSE-friendly).
    pub idle_timeout_secs: u64,
    /// Permit `http` / `ws` (plaintext) upstream connections. `false` keeps
    /// the default HTTPS-only posture of
    /// `cpt-cf-oagw-constraint-https-only`.
    pub allow_http_upstream: bool,
    /// Maximum buffered request body. Capped at [`BODY_LIMIT_BYTES`].
    pub max_body_bytes: usize,
    /// SSRF guard policy.
    pub ssrf_policy: SsrfPolicy,
    /// Ceiling for cached OAuth2 access tokens, in seconds.
    pub token_cache_ttl_secs: u64,
    /// Maximum entries in the OAuth2 token cache.
    pub token_cache_capacity: usize,
    /// Data-plane L1 config cache capacity (`cpt-cf-oagw-adr-state-management`).
    pub l1_cache_capacity: usize,
    /// TTL, in days, after which an unlinked custom plugin becomes eligible
    /// for garbage collection.
    pub plugin_gc_ttl_days: u64,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: 30,
            connect_timeout_secs: 10,
            idle_timeout_secs: 300,
            allow_http_upstream: false,
            max_body_bytes: BODY_LIMIT_BYTES,
            ssrf_policy: SsrfPolicy::default(),
            token_cache_ttl_secs: 300,
            token_cache_capacity: 10_000,
            l1_cache_capacity: 1_000,
            plugin_gc_ttl_days: 30,
        }
    }
}

impl OagwConfig {
    /// Validate the configuration, clamping values that only make sense
    /// inside a range rather than failing the whole gear.
    ///
    /// # Errors
    ///
    /// Returns a human-readable message when a value cannot be repaired.
    pub fn validate(&mut self) -> Result<(), String> {
        if self.proxy_timeout_secs == 0 {
            return Err("proxy_timeout_secs must be greater than zero".to_owned());
        }
        if self.connect_timeout_secs == 0 {
            return Err("connect_timeout_secs must be greater than zero".to_owned());
        }
        if self.max_body_bytes == 0 || self.max_body_bytes > BODY_LIMIT_BYTES {
            self.max_body_bytes = BODY_LIMIT_BYTES;
        }
        if self.l1_cache_capacity == 0 {
            self.l1_cache_capacity = 1;
        }
        if self.token_cache_capacity == 0 {
            self.token_cache_capacity = 1;
        }
        Ok(())
    }

    /// Overall proxy deadline.
    #[must_use]
    pub fn proxy_timeout(&self) -> Duration {
        Duration::from_secs(self.proxy_timeout_secs)
    }

    /// Connection-establishment deadline.
    #[must_use]
    pub fn connect_timeout(&self) -> Duration {
        Duration::from_secs(self.connect_timeout_secs)
    }

    /// Per-read deadline while draining an upstream response body.
    #[must_use]
    pub fn idle_timeout(&self) -> Duration {
        Duration::from_secs(self.idle_timeout_secs.max(1))
    }

    /// OAuth2 token-cache settings.
    #[must_use]
    pub fn token_cache(&self) -> TokenCacheConfig {
        TokenCacheConfig {
            ttl: Duration::from_secs(self.token_cache_ttl_secs.max(1)),
            capacity: self.token_cache_capacity,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_secure() {
        let cfg = OagwConfig::default();
        assert!(
            !cfg.allow_http_upstream,
            "plaintext upstreams denied by default"
        );
        assert!(cfg.ssrf_policy.enabled, "ssrf guard on by default");
        assert_eq!(cfg.max_body_bytes, BODY_LIMIT_BYTES);
    }

    #[test]
    fn deserializes_the_e2e_block() {
        let cfg: OagwConfig = serde_json::from_value(serde_json::json!({
            "proxy_timeout_secs": 2,
            "allow_http_upstream": true,
            "ssrf_policy": { "enabled": false }
        }))
        .expect("config parses");
        assert_eq!(cfg.proxy_timeout_secs, 2);
        assert!(cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.enabled);
        // Unspecified fields keep their defaults.
        assert_eq!(cfg.token_cache_ttl_secs, 300);
    }

    #[test]
    fn validate_clamps_body_limit() {
        let mut cfg = OagwConfig {
            max_body_bytes: usize::MAX,
            ..OagwConfig::default()
        };
        cfg.validate().expect("valid");
        assert_eq!(cfg.max_body_bytes, BODY_LIMIT_BYTES);
    }

    #[test]
    fn validate_rejects_zero_timeout() {
        let mut cfg = OagwConfig {
            proxy_timeout_secs: 0,
            ..OagwConfig::default()
        };
        assert!(cfg.validate().is_err());
    }
}
