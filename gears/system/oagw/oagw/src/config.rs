//! Gear-level configuration for the Outbound API Gateway (OAGW).
//!
//! Mirrors the `gears.oagw.config` section of the server configuration. Every
//! field is optional so an operator can deploy the gear with no `config`
//! block at all and still get the documented defaults.

use std::time::Duration;

use serde::Deserialize;

/// Hard ceiling on a proxied request body, per
/// `cpt-cf-oagw-constraint-body-limit` (100 MB).
pub const BODY_LIMIT_BYTES: usize = 100 * 1024 * 1024;

/// Default upstream request timeout when the operator does not set one.
const DEFAULT_PROXY_TIMEOUT_SECS: u64 = 30;

/// Default ceiling for cached OAuth2 access tokens (ADR-0008).
const DEFAULT_TOKEN_CACHE_TTL_SECS: u64 = 300;

/// Default OAuth2 token cache capacity (ADR-0008).
const DEFAULT_TOKEN_CACHE_CAPACITY: usize = 10_000;

/// Default Data Plane L1 config cache capacity (ADR-0006).
const DEFAULT_L1_CACHE_CAPACITY: usize = 1_000;

/// Default TTL after which an unlinked custom plugin becomes GC-eligible.
const DEFAULT_PLUGIN_GC_TTL_DAYS: u64 = 30;

/// Default TTL of the tenant-hierarchy (ancestor chain) cache.
const DEFAULT_ANCESTOR_CACHE_TTL_SECS: u64 = 30;

/// Configuration of the SSRF guard applied before an upstream connection is
/// made (`cpt-cf-oagw-nfr-ssrf-protection`).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SsrfPolicyConfig {
    /// Master switch. Enabled by default: a deployment must opt *out* of
    /// SSRF protection, never opt in.
    pub enabled: bool,
    /// Allow resolved addresses inside loopback / private / link-local /
    /// unique-local ranges. Only consulted when [`Self::enabled`].
    pub allow_private_networks: bool,
    /// Extra host names that bypass the policy entirely (exact, ASCII
    /// lowercase match on the endpoint host).
    pub allowed_hosts: Vec<String>,
}

impl Default for SsrfPolicyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allow_private_networks: false,
            allowed_hosts: Vec::new(),
        }
    }
}

/// Gear-level OAGW configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct OagwConfig {
    /// Time budget for obtaining upstream response headers. Streaming bodies
    /// (SSE, chunked, upgraded connections) are not bounded by it once the
    /// response head has been received.
    pub proxy_timeout_secs: u64,
    /// Time budget for establishing the upstream transport connection.
    /// Defaults to [`Self::proxy_timeout_secs`] when omitted.
    pub connect_timeout_secs: Option<u64>,
    /// Permit plaintext (`http` / `ws`) upstream connections.
    ///
    /// This governs *only* whether a plaintext connection is actually made;
    /// which schemes the `server.endpoints[].scheme` field accepts is a
    /// separate, always-permissive question (see
    /// `cpt-cf-oagw-constraint-https-only`).
    pub allow_http_upstream: bool,
    /// SSRF guard settings.
    pub ssrf_policy: SsrfPolicyConfig,
    /// Ceiling for cached OAuth2 client-credentials access tokens.
    pub token_cache_ttl_secs: u64,
    /// Maximum number of cached OAuth2 access tokens.
    pub token_cache_capacity: usize,
    /// Data Plane L1 resolved-config cache capacity.
    pub l1_cache_capacity: usize,
    /// TTL after which an unlinked custom plugin becomes GC-eligible.
    pub plugin_gc_ttl_days: u64,
    /// TTL of the cached tenant ancestor chain.
    pub ancestor_cache_ttl_secs: u64,
    /// Hard request-body ceiling. Requests declaring more are rejected with
    /// `413 PayloadTooLarge` before any buffering happens.
    pub max_body_bytes: usize,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: DEFAULT_PROXY_TIMEOUT_SECS,
            connect_timeout_secs: None,
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicyConfig::default(),
            token_cache_ttl_secs: DEFAULT_TOKEN_CACHE_TTL_SECS,
            token_cache_capacity: DEFAULT_TOKEN_CACHE_CAPACITY,
            l1_cache_capacity: DEFAULT_L1_CACHE_CAPACITY,
            plugin_gc_ttl_days: DEFAULT_PLUGIN_GC_TTL_DAYS,
            ancestor_cache_ttl_secs: DEFAULT_ANCESTOR_CACHE_TTL_SECS,
            max_body_bytes: BODY_LIMIT_BYTES,
        }
    }
}

impl OagwConfig {
    /// Validate operator-supplied values.
    ///
    /// # Errors
    ///
    /// Returns a human-readable message when a value cannot produce a working
    /// gear (zero timeouts, zero-sized caches, oversized body ceiling).
    pub fn validate(&self) -> Result<(), String> {
        if self.proxy_timeout_secs == 0 {
            return Err("proxy_timeout_secs must be greater than zero".to_owned());
        }
        if self.connect_timeout_secs == Some(0) {
            return Err("connect_timeout_secs must be greater than zero".to_owned());
        }
        if self.token_cache_capacity == 0 {
            return Err("token_cache_capacity must be greater than zero".to_owned());
        }
        if self.l1_cache_capacity == 0 {
            return Err("l1_cache_capacity must be greater than zero".to_owned());
        }
        if self.max_body_bytes == 0 || self.max_body_bytes > BODY_LIMIT_BYTES {
            return Err(format!(
                "max_body_bytes must be in 1..={BODY_LIMIT_BYTES} (the 100MB hard limit)"
            ));
        }
        Ok(())
    }

    /// Response-header timeout as a [`Duration`].
    #[must_use]
    pub fn proxy_timeout(&self) -> Duration {
        Duration::from_secs(self.proxy_timeout_secs)
    }

    /// Connect timeout as a [`Duration`], falling back to the proxy timeout.
    #[must_use]
    pub fn connect_timeout(&self) -> Duration {
        Duration::from_secs(self.connect_timeout_secs.unwrap_or(self.proxy_timeout_secs))
    }

    /// OAuth2 token cache TTL ceiling as a [`Duration`].
    #[must_use]
    pub fn token_cache_ttl(&self) -> Duration {
        Duration::from_secs(self.token_cache_ttl_secs)
    }

    /// Tenant ancestor-chain cache TTL as a [`Duration`].
    #[must_use]
    pub fn ancestor_cache_ttl(&self) -> Duration {
        Duration::from_secs(self.ancestor_cache_ttl_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::{BODY_LIMIT_BYTES, OagwConfig};

    #[test]
    fn defaults_are_secure() {
        let cfg = OagwConfig::default();
        assert!(!cfg.allow_http_upstream, "plaintext must be opt-in");
        assert!(cfg.ssrf_policy.enabled, "ssrf guard must be opt-out");
        assert_eq!(cfg.max_body_bytes, BODY_LIMIT_BYTES);
        cfg.validate().expect("defaults validate");
    }

    #[test]
    fn e2e_shaped_config_parses() {
        let cfg: OagwConfig = serde_json::from_value(serde_json::json!({
            "proxy_timeout_secs": 2,
            "allow_http_upstream": true,
            "ssrf_policy": { "enabled": false }
        }))
        .expect("config parses");
        assert_eq!(cfg.proxy_timeout_secs, 2);
        assert!(cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.enabled);
        // Untouched fields keep their documented defaults.
        assert_eq!(cfg.token_cache_ttl_secs, 300);
        cfg.validate().expect("valid");
    }

    #[test]
    fn unknown_keys_are_tolerated() {
        let cfg: OagwConfig =
            serde_json::from_value(serde_json::json!({ "future_option": 7 })).expect("lenient");
        assert_eq!(cfg.proxy_timeout_secs, 30);
    }

    #[test]
    fn zero_timeout_is_rejected() {
        let cfg = OagwConfig {
            proxy_timeout_secs: 0,
            ..OagwConfig::default()
        };
        assert!(cfg.validate().is_err());
    }
}
