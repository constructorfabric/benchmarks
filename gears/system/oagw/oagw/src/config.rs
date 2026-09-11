//! Gear-level configuration (`gears.oagw.config` in the server YAML).

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Hard ceiling on a proxied request body, per
/// `cpt-cf-oagw-constraint-body-limit`. Requests declaring more are rejected
/// with `413` before any byte is buffered.
pub const BODY_LIMIT_BYTES: u64 = 100 * 1024 * 1024;

/// Default wall-clock budget for a single upstream exchange.
const DEFAULT_PROXY_TIMEOUT_SECS: u64 = 30;
/// Default L1 (Data Plane) resolved-configuration cache size — ADR 0006.
const DEFAULT_L1_CACHE_ENTRIES: usize = 1000;
/// Default OAuth2 access-token cache ceiling — ADR 0008.
const DEFAULT_TOKEN_CACHE_TTL_SECS: u64 = 300;
/// Default OAuth2 access-token cache capacity — ADR 0008.
const DEFAULT_TOKEN_CACHE_CAPACITY: usize = 10_000;
/// Default TTL after which an unlinked custom plugin becomes collectable.
const DEFAULT_PLUGIN_GC_TTL_SECS: u64 = 30 * 24 * 60 * 60;

/// SSRF guard rails applied to every resolved upstream endpoint.
///
/// `cpt-cf-oagw-nfr-ssrf-protection` requires DNS results to be validated
/// before a connection is made. When [`enabled`](Self::enabled) is `false`
/// the address checks are skipped entirely — that is what a local/E2E
/// deployment needs in order to reach a loopback mock server.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct SsrfPolicyConfig {
    /// Master switch. Disabled deployments perform no address filtering.
    pub enabled: bool,
    /// Reject endpoints resolving to loopback addresses.
    pub block_loopback: bool,
    /// Reject endpoints resolving to RFC 1918 / RFC 4193 private ranges.
    pub block_private: bool,
    /// Reject endpoints resolving to link-local addresses (includes the
    /// cloud metadata service at `169.254.169.254`).
    pub block_link_local: bool,
    /// Literal hosts that bypass the checks above (already normalized to
    /// lowercase on read).
    pub allow_hosts: Vec<String>,
}

impl Default for SsrfPolicyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            block_loopback: true,
            block_private: true,
            block_link_local: true,
            allow_hosts: Vec::new(),
        }
    }
}

impl SsrfPolicyConfig {
    /// `true` when `host` is exempt from address filtering.
    #[must_use]
    pub fn is_allowlisted(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        self.allow_hosts.iter().any(|h| h.eq_ignore_ascii_case(&host))
    }
}

/// `gears.oagw.config`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct OagwConfig {
    /// Wall-clock budget for one upstream exchange (connect + headers). A
    /// breach surfaces as `504` with the request-timeout GTS type.
    pub proxy_timeout_secs: u64,
    /// Connect-only budget. Defaults to `proxy_timeout_secs` when unset.
    pub connect_timeout_secs: Option<u64>,
    /// Permit plaintext (`http`/`ws`) connections to upstreams.
    ///
    /// `cpt-cf-oagw-constraint-https-only` is the *default* posture: without
    /// this flag a plaintext endpoint is accepted by the management API (the
    /// scheme is a legal field value) but refused at connect time. The two
    /// layers are deliberately separate — see `docs/DESIGN.md` §2.2.
    pub allow_http_upstream: bool,
    /// Address filtering for resolved endpoints.
    pub ssrf_policy: SsrfPolicyConfig,
    /// Entries kept in the Data Plane L1 resolved-config cache (ADR 0006).
    pub l1_cache_entries: usize,
    /// Ceiling for a cached OAuth2 access token (ADR 0008).
    pub token_cache_ttl_secs: u64,
    /// Maximum entries in the OAuth2 access-token cache (ADR 0008).
    pub token_cache_capacity: usize,
    /// How long an unlinked custom plugin lingers before it is collectable.
    pub plugin_gc_ttl_secs: u64,
    /// Register the GTS type catalog with the types-registry on startup.
    pub provision_types: bool,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: DEFAULT_PROXY_TIMEOUT_SECS,
            connect_timeout_secs: None,
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicyConfig::default(),
            l1_cache_entries: DEFAULT_L1_CACHE_ENTRIES,
            token_cache_ttl_secs: DEFAULT_TOKEN_CACHE_TTL_SECS,
            token_cache_capacity: DEFAULT_TOKEN_CACHE_CAPACITY,
            plugin_gc_ttl_secs: DEFAULT_PLUGIN_GC_TTL_SECS,
            provision_types: true,
        }
    }
}

impl OagwConfig {
    /// Reject values that would produce a nonsensical runtime.
    ///
    /// # Errors
    ///
    /// Returns a human-readable message when a duration or capacity is zero.
    pub fn validate(&self) -> Result<(), String> {
        if self.proxy_timeout_secs == 0 {
            return Err("proxy_timeout_secs must be greater than zero".to_owned());
        }
        if self.connect_timeout_secs == Some(0) {
            return Err("connect_timeout_secs must be greater than zero".to_owned());
        }
        if self.l1_cache_entries == 0 {
            return Err("l1_cache_entries must be greater than zero".to_owned());
        }
        if self.token_cache_capacity == 0 {
            return Err("token_cache_capacity must be greater than zero".to_owned());
        }
        Ok(())
    }

    /// Wall-clock budget for one upstream exchange.
    #[must_use]
    pub fn proxy_timeout(&self) -> Duration {
        Duration::from_secs(self.proxy_timeout_secs)
    }

    /// Connect-only budget, falling back to [`Self::proxy_timeout`].
    #[must_use]
    pub fn connect_timeout(&self) -> Duration {
        Duration::from_secs(self.connect_timeout_secs.unwrap_or(self.proxy_timeout_secs))
    }

    /// Ceiling for a cached OAuth2 access token.
    #[must_use]
    pub fn token_cache_ttl(&self) -> Duration {
        Duration::from_secs(self.token_cache_ttl_secs)
    }

    /// TTL after which an unlinked custom plugin may be collected.
    #[must_use]
    pub fn plugin_gc_ttl(&self) -> Duration {
        Duration::from_secs(self.plugin_gc_ttl_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_https_only_with_ssrf_on() {
        let cfg = OagwConfig::default();
        assert!(!cfg.allow_http_upstream);
        assert!(cfg.ssrf_policy.enabled);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn e2e_shaped_yaml_deserializes() {
        let json = serde_json::json!({
            "proxy_timeout_secs": 2,
            "allow_http_upstream": true,
            "ssrf_policy": { "enabled": false }
        });
        let cfg: OagwConfig = serde_json::from_value(json).expect("config parses");
        assert_eq!(cfg.proxy_timeout_secs, 2);
        assert!(cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.enabled);
        // Unset keys keep their documented defaults.
        assert_eq!(cfg.token_cache_ttl_secs, DEFAULT_TOKEN_CACHE_TTL_SECS);
        assert_eq!(cfg.token_cache_capacity, DEFAULT_TOKEN_CACHE_CAPACITY);
    }

    #[test]
    fn zero_timeout_is_rejected() {
        let cfg = OagwConfig {
            proxy_timeout_secs: 0,
            ..OagwConfig::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn connect_timeout_falls_back_to_proxy_timeout() {
        let cfg = OagwConfig {
            proxy_timeout_secs: 7,
            ..OagwConfig::default()
        };
        assert_eq!(cfg.connect_timeout(), Duration::from_secs(7));
    }
}
