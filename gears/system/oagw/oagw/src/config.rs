//! Gear-level configuration for the OAGW gear.

use serde::Deserialize;

/// Server-side request forgery policy.
///
/// When enabled, upstream endpoint hosts must resolve to a public address
/// space: private, loopback, link-local and other non-routable segments are
/// rejected at proxy time. Disabled only for local development.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SsrfPolicyConfig {
    /// Reject upstream hosts that resolve into private / non-routable ranges.
    pub enabled: bool,
}

impl Default for SsrfPolicyConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Default ceiling for a cached OAuth2 access token, in seconds (ADR-0008).
pub const TOKEN_CACHE_TTL_SECS: u64 = 300;

/// Default maximum number of entries in the OAuth2 token cache (ADR-0008).
pub const TOKEN_CACHE_CAPACITY: usize = 10_000;

/// OAGW gear configuration.
///
/// Loaded from the `gears.oagw.config` section of the deployment config.
/// Upstream HTTPS is mandatory unless `allow_http_upstream` is explicitly
/// turned on (test/local deployments only).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OagwConfig {
    /// Overall proxy timeout applied to the upstream call, in seconds. Covers
    /// the connection phase (`timeout.connection.v1`) and the response-head
    /// phase (`timeout.request.v1`); the same value is reused as the idle
    /// timeout for streamed response bodies (`timeout.idle.v1`).
    pub proxy_timeout_secs: u64,
    /// Allow plaintext `http://` upstream connections. Rejecting plaintext
    /// upstreams is the MVP security default (`cpt-cf-oagw-constraint-https-only`).
    pub allow_http_upstream: bool,
    /// SSRF guard configuration.
    pub ssrf_policy: SsrfPolicyConfig,
    /// Ceiling for a cached OAuth2 access token, in seconds. The effective
    /// TTL is `min(token_cache_ttl_secs, expires_in - safety margin)`
    /// (ADR-0008).
    pub token_cache_ttl_secs: u64,
    /// Maximum number of entries in the OAuth2 token cache (ADR-0008).
    pub token_cache_capacity: usize,
    /// Reverse proxies in front of the gateway whose `Forwarded` /
    /// `X-Forwarded-For` headers may be believed, as bare IPs or CIDR ranges
    /// (`10.0.0.0/8`, `fd00::/8`). Empty by default, which makes the gateway
    /// rate-limit on its own view of the peer address and ignore
    /// client-supplied forwarding headers entirely.
    pub trusted_proxies: Vec<String>,
}

impl OagwConfig {
    /// Ceiling for a cached OAuth2 access token, in seconds (ADR-0008).
    ///
    /// The OAuth2 plugin currently consumes the ADR-0008 default as a
    /// build-time constant; this accessor is the seam for threading the
    /// configured value through to the plugin layer.
    #[must_use]
    pub fn token_cache_ttl_secs(&self) -> u64 {
        self.token_cache_ttl_secs
    }

    /// Maximum number of entries in the OAuth2 token cache (ADR-0008).
    #[must_use]
    pub fn token_cache_capacity(&self) -> usize {
        self.token_cache_capacity
    }

    /// Reverse proxies whose forwarding headers may be believed.
    ///
    /// Unparsable entries are dropped rather than rejected so that a bad entry
    /// cannot take the whole gear down; the data plane re-parses and logs the
    /// entries it drops.
    #[must_use]
    pub fn trusted_proxies(&self) -> Vec<String> {
        self.trusted_proxies.clone()
    }
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: 30,
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicyConfig::default(),
            token_cache_ttl_secs: TOKEN_CACHE_TTL_SECS,
            token_cache_capacity: TOKEN_CACHE_CAPACITY,
            trusted_proxies: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_safe() {
        let config = OagwConfig::default();
        assert_eq!(config.proxy_timeout_secs, 30);
        assert!(!config.allow_http_upstream);
        assert!(config.ssrf_policy.enabled);
        assert!(config.trusted_proxies().is_empty());
    }

    #[test]
    fn the_config_section_accepts_the_documented_keys() {
        let parsed: OagwConfig = serde_json::from_value(serde_json::json!({
            "proxy_timeout_secs": 5,
            "allow_http_upstream": true,
            "ssrf_policy": { "enabled": false },
            "token_cache_ttl_secs": 60,
            "token_cache_capacity": 100,
            "trusted_proxies": ["10.0.0.0/8", "fd00::/8"],
        }))
        .expect("valid config section");
        assert_eq!(parsed.proxy_timeout_secs, 5);
        assert!(parsed.allow_http_upstream);
        assert!(!parsed.ssrf_policy.enabled);
        assert_eq!(parsed.token_cache_ttl_secs(), 60);
        assert_eq!(parsed.token_cache_capacity(), 100);
        assert_eq!(
            parsed.trusted_proxies(),
            vec!["10.0.0.0/8".to_owned(), "fd00::/8".to_owned()]
        );
    }

    #[test]
    fn an_unknown_key_is_rejected() {
        let error = serde_json::from_value::<OagwConfig>(serde_json::json!({
            "proxy_timeout_secs": 5,
            "not_a_key": 1,
        }))
        .expect_err("unknown key");
        assert!(
            error.to_string().contains("unknown field"),
            "unexpected error: {error}"
        );
    }
}
