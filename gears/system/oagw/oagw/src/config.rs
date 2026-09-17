//! OAGW gear configuration.
//!
//! The configuration surface is deliberately minimal: only the keys that the
//! gear actually consumes are declared, everything else is defaulted so that a
//! partial `gears.oagw.config` block (see `/app/config/e2e-local.yaml`)
//! deserializes. Unknown keys are rejected — a typo such as
//! `proxy_timeouts_secs` must fail loudly at startup instead of being silently
//! ignored while the operator believes the value took effect.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Default outbound proxy timeout in seconds (`proxy_timeout_secs`).
fn default_proxy_timeout_secs() -> u64 {
    30
}

/// Default OAuth2 token cache entry TTL in seconds (ADR-0008).
fn default_token_cache_ttl_secs() -> u64 {
    300
}

/// Default OAuth2 token cache capacity in entries (ADR-0008).
fn default_token_cache_capacity() -> u32 {
    10_000
}

/// Default number of rate-limit buckets the data plane keeps (ADR-0003).
fn default_rate_limit_bucket_capacity() -> usize {
    crate::domain::policy::rate_limit::DEFAULT_BUCKET_CAPACITY
}

/// Server-side request forgery (SSRF) guard configuration.
///
/// Only the master switch is declared for now; the DNS/IP-pinning rules that
/// make up the rest of the SSRF policy are introduced by the data-plane slice
/// (DESIGN §4.4) and will be added as new fields here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SsrfPolicy {
    /// Enable SSRF request validation before dialing an upstream.
    #[serde(default)]
    pub enabled: bool,
}

impl SsrfPolicy {
    /// `true` when the SSRF guard validates requests before dialing.
    #[must_use]
    pub const fn is_enabled(self) -> bool {
        self.enabled
    }
}

/// OAGW gear configuration (`gears.oagw.config`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OagwConfig {
    /// Outbound proxy timeout in seconds (data-plane request deadline).
    #[serde(default = "default_proxy_timeout_secs")]
    pub proxy_timeout_secs: u64,

    /// Allow plaintext (`http://`) connections to upstream services.
    ///
    /// `scheme` validation on the control plane is independent of this flag:
    /// `http` is always an accepted *endpoint* scheme, this flag only decides
    /// whether the data plane is actually allowed to open a plaintext socket.
    /// Production must leave this `false` (DESIGN §2.2 HTTPS-only constraint).
    #[serde(default)]
    pub allow_http_upstream: bool,

    /// SSRF guard configuration.
    #[serde(default)]
    pub ssrf_policy: SsrfPolicy,

    /// OAuth2 Client Credentials token cache TTL in seconds (ADR-0008).
    #[serde(default = "default_token_cache_ttl_secs")]
    pub token_cache_ttl_secs: u64,

    /// OAuth2 Client Credentials token cache capacity in entries (ADR-0008).
    #[serde(default = "default_token_cache_capacity")]
    pub token_cache_capacity: u32,

    /// Most rate-limit buckets the data plane keeps (ADR-0003).
    ///
    /// A `scope: ip` counter is keyed on an address the caller supplies in
    /// `X-Forwarded-For`, so the bucket map has to have a ceiling: once it is
    /// full, an expired bucket is dropped first and otherwise the least
    /// recently updated one, which is what keeps one caller minting fresh
    /// addresses from growing shared memory without limit. See
    /// [`crate::domain::policy::rate_limit::RateLimitLimiter`].
    #[serde(default = "default_rate_limit_bucket_capacity")]
    pub rate_limit_bucket_capacity: usize,
}

impl Default for OagwConfig {
    // Manual (not derived) so defaults match the `#[serde(default = ...)]`
    // functions above; a derived `Default` would zero every field, and a zero
    // proxy timeout would fail every outbound request.
    fn default() -> Self {
        Self {
            proxy_timeout_secs: default_proxy_timeout_secs(),
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicy::default(),
            token_cache_ttl_secs: default_token_cache_ttl_secs(),
            token_cache_capacity: default_token_cache_capacity(),
            rate_limit_bucket_capacity: default_rate_limit_bucket_capacity(),
        }
    }
}

impl OagwConfig {
    /// Validate the configuration at load time.
    ///
    /// Zero-valued timeouts and cache sizes are configuration mistakes that
    /// would otherwise surface as runtime failures on every request (instant
    /// timeouts, cache that holds nothing).
    ///
    /// # Errors
    /// Returns a human-readable description of the first invalid field.
    pub fn validate(&self) -> Result<(), String> {
        if self.proxy_timeout_secs == 0 {
            return Err(
                "invalid oagw configuration: `proxy_timeout_secs` must be at least 1".to_owned(),
            );
        }
        if self.token_cache_ttl_secs == 0 {
            return Err(
                "invalid oagw configuration: `token_cache_ttl_secs` must be at least 1".to_owned(),
            );
        }
        if self.token_cache_capacity == 0 {
            return Err(
                "invalid oagw configuration: `token_cache_capacity` must be at least 1".to_owned(),
            );
        }
        if self.rate_limit_bucket_capacity == 0 {
            return Err(
                "invalid oagw configuration: `rate_limit_bucket_capacity` must be at least 1"
                    .to_owned(),
            );
        }
        Ok(())
    }

    /// Outbound proxy timeout as a [`Duration`].
    #[must_use]
    pub const fn proxy_timeout(&self) -> Duration {
        Duration::from_secs(self.proxy_timeout_secs)
    }

    /// `true` when the data plane may open a plaintext connection to an
    /// upstream whose endpoint scheme is `http`.
    #[must_use]
    pub const fn allows_http_upstream(&self) -> bool {
        self.allow_http_upstream
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact `gears.oagw.config` block shipped in `/app/config/e2e-local.yaml`
    /// (`proxy_timeout_secs: 2`, `allow_http_upstream: true`,
    /// `ssrf_policy.enabled: false`), serialized as JSON.
    ///
    /// Serde mapping rules (defaults, `deny_unknown_fields`, nested objects) are
    /// format-agnostic, so asserting on JSON exercises exactly the same
    /// deserialization logic the YAML loader feeds into. No YAML parser is a
    /// declared dependency of this crate, so the YAML *syntax* itself is
    /// exercised by the e2e suite instead.
    #[test]
    fn parses_e2e_local_config_keys() {
        let raw = r#"{
            "proxy_timeout_secs": 2,
            "allow_http_upstream": true,
            "ssrf_policy": { "enabled": false }
        }"#;
        let cfg: OagwConfig = serde_json::from_str(raw).expect("e2e config keys must parse");

        assert_eq!(cfg.proxy_timeout_secs, 2);
        assert!(cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.is_enabled());
        // ADR-0008 keys default when absent from the config file.
        assert_eq!(cfg.token_cache_ttl_secs, 300);
        assert_eq!(cfg.token_cache_capacity, 10_000);
        assert_eq!(
            cfg.rate_limit_bucket_capacity,
            crate::domain::policy::rate_limit::DEFAULT_BUCKET_CAPACITY,
            "the rate-limit ceiling defaults too"
        );
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn parses_empty_config_object_with_all_defaults() {
        let cfg: OagwConfig = serde_json::from_str("{}").expect("empty config must parse");

        assert_eq!(cfg, OagwConfig::default());
        assert_eq!(cfg.proxy_timeout(), Duration::from_secs(30));
        assert!(!cfg.allows_http_upstream());
        assert!(!cfg.ssrf_policy.is_enabled());
    }

    #[test]
    fn defaults_match_documented_values() {
        let cfg = OagwConfig::default();

        assert_eq!(
            cfg,
            OagwConfig {
                proxy_timeout_secs: 30,
                allow_http_upstream: false,
                ssrf_policy: SsrfPolicy { enabled: false },
                token_cache_ttl_secs: 300,
                token_cache_capacity: 10_000,
                rate_limit_bucket_capacity: 65_536,
            }
        );
        assert_eq!(cfg.proxy_timeout(), Duration::from_secs(30));
        assert!(!cfg.allows_http_upstream());
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn rejects_unknown_keys() {
        let parsed: Result<OagwConfig, _> =
            serde_json::from_str(r#"{ "proxy_timeouts_secs": 30 }"#);

        assert!(parsed.is_err(), "typos in config keys must fail loudly");
    }

    #[test]
    fn rejects_unknown_nested_keys() {
        let parsed: Result<OagwConfig, _> =
            serde_json::from_str(r#"{ "ssrf_policy": { "allowed_cidrs": [] } }"#);

        assert!(parsed.is_err());
    }

    #[test]
    fn rejects_zero_timeouts_and_cache_sizes() {
        for cfg in [
            OagwConfig {
                proxy_timeout_secs: 0,
                ..OagwConfig::default()
            },
            OagwConfig {
                token_cache_ttl_secs: 0,
                ..OagwConfig::default()
            },
            OagwConfig {
                token_cache_capacity: 0,
                ..OagwConfig::default()
            },
            OagwConfig {
                rate_limit_bucket_capacity: 0,
                ..OagwConfig::default()
            },
        ] {
            assert!(
                cfg.validate().is_err(),
                "zero-valued config must be rejected"
            );
        }
    }

    #[test]
    fn a_zero_rate_limit_bucket_capacity_is_rejected_by_name() {
        let cfg = OagwConfig {
            rate_limit_bucket_capacity: 0,
            ..OagwConfig::default()
        };

        assert_eq!(
            cfg.validate().expect_err("zero capacity is a mistake"),
            "invalid oagw configuration: `rate_limit_bucket_capacity` must be at least 1",
            "the message names the field, as the other ceilings do"
        );
    }

    #[test]
    fn the_rate_limit_bucket_capacity_is_configurable() {
        let cfg: OagwConfig =
            serde_json::from_str(r#"{ "rate_limit_bucket_capacity": 128 }"#).expect("parses");

        assert_eq!(cfg.rate_limit_bucket_capacity, 128);
        assert!(cfg.validate().is_ok());
    }
}
