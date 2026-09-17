//! OAGW gear configuration, read from the `gears.oagw.config` YAML block.
//!
//! Every key is optional and falls back to the default documented in
//! `docs/DESIGN.md` and `docs/ADR/0008`; unknown keys are rejected so that
//! operator typos fail fast at startup instead of silently disabling a
//! protection.

use serde::Deserialize;

/// Default per-request proxy timeout (30 s).
const DEFAULT_PROXY_TIMEOUT_SECS: u64 = 30;
/// Default ceiling for cached OAuth2 client-credentials tokens (ADR-0008).
const DEFAULT_TOKEN_CACHE_TTL_SECS: u64 = 300;
/// Default capacity of the OAuth2 client-credentials token cache (ADR-0008).
const DEFAULT_TOKEN_CACHE_CAPACITY: usize = 10_000;

/// OAGW gear configuration.
///
/// Deserialized with `ctx.config_or_default::<OagwConfig>()` from the
/// `gears.oagw.config` section of the server configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OagwConfig {
    /// Per-request proxy timeout in seconds. Covers DNS, connection, upstream
    /// response headers and body streaming.
    pub proxy_timeout_secs: u64,
    /// Whether plaintext `http://` upstream endpoints may actually be dialed.
    ///
    /// `http` is always a *legal* endpoint scheme in the schema; this flag only
    /// governs whether the data plane is allowed to open such a connection.
    pub allow_http_upstream: bool,
    /// SSRF protection switches applied to resolved upstream targets.
    pub ssrf_policy: SsrfPolicyConfig,
    /// Ceiling (seconds) for cached OAuth2 access tokens (ADR-0008). The
    /// effective TTL is `min(this, expires_in - 30s)`.
    pub token_cache_ttl_secs: u64,
    /// Maximum number of entries in the OAuth2 token cache (ADR-0008).
    pub token_cache_capacity: usize,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: DEFAULT_PROXY_TIMEOUT_SECS,
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicyConfig::default(),
            token_cache_ttl_secs: DEFAULT_TOKEN_CACHE_TTL_SECS,
            token_cache_capacity: DEFAULT_TOKEN_CACHE_CAPACITY,
        }
    }
}

impl OagwConfig {
    /// Validates the configuration after deserialization.
    ///
    /// Called from `Gear::init`; a failure aborts gear startup.
    ///
    /// # Errors
    /// Returns a human-readable description of the first invalid value.
    pub fn validate(&self) -> Result<(), String> {
        if self.proxy_timeout_secs == 0 {
            return Err("proxy_timeout_secs must be greater than 0".to_owned());
        }
        if self.token_cache_ttl_secs == 0 {
            return Err("token_cache_ttl_secs must be greater than 0".to_owned());
        }
        if self.token_cache_capacity == 0 {
            return Err("token_cache_capacity must be greater than 0".to_owned());
        }
        Ok(())
    }
}

/// SSRF protection switches for the proxy data plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SsrfPolicyConfig {
    /// Whether upstream targets that resolve into private, loopback or
    /// link-local address space are rejected. On by default: egress to
    /// infrastructure-internal addresses is only ever needed by tests.
    pub enabled: bool,
}

impl Default for SsrfPolicyConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use serde_json::json;

    use super::{OagwConfig, SsrfPolicyConfig};

    #[test]
    fn defaults_match_documented_values() {
        let config = OagwConfig::default();
        assert_eq!(config.proxy_timeout_secs, 30);
        assert!(!config.allow_http_upstream);
        assert!(config.ssrf_policy.enabled);
        assert_eq!(config.token_cache_ttl_secs, 300);
        assert_eq!(config.token_cache_capacity, 10_000);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn deserializes_e2e_local_config_block() {
        // Keys and values copied verbatim from the `gears.oagw.config` block in
        // `config/e2e-local.yaml` (same serde data model, JSON spelling).
        let block = json!({
            "proxy_timeout_secs": 2,
            "allow_http_upstream": true,
            "ssrf_policy": { "enabled": false },
        });

        let config: OagwConfig = serde_json::from_value(block).expect("deserialize e2e config");
        assert_eq!(config.proxy_timeout_secs, 2);
        assert!(config.allow_http_upstream);
        assert!(!config.ssrf_policy.enabled);
        // ADR-0008 knobs keep their documented defaults.
        assert_eq!(config.token_cache_ttl_secs, 300);
        assert_eq!(config.token_cache_capacity, 10_000);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn rejects_unknown_config_key() {
        let block = json!({ "proxy_timeout_secs": 2, "unknown_key": true });
        let error = serde_json::from_value::<OagwConfig>(block).unwrap_err();
        assert!(
            error.to_string().contains("unknown_key"),
            "expected an unknown-field error, got: {error}"
        );
    }

    #[test]
    fn rejects_unknown_ssrf_policy_key() {
        let block = json!({ "ssrf_policy": { "enabled": true, "bogus": 1 } });
        let error = serde_json::from_value::<OagwConfig>(block).unwrap_err();
        assert!(error.to_string().contains("bogus"), "got: {error}");
    }

    #[test]
    fn validate_rejects_each_zero_knob() {
        let config = OagwConfig {
            proxy_timeout_secs: 0,
            ..OagwConfig::default()
        };
        assert_eq!(
            config.validate().unwrap_err(),
            "proxy_timeout_secs must be greater than 0"
        );

        let config = OagwConfig {
            token_cache_ttl_secs: 0,
            ..OagwConfig::default()
        };
        assert_eq!(
            config.validate().unwrap_err(),
            "token_cache_ttl_secs must be greater than 0"
        );

        let config = OagwConfig {
            token_cache_capacity: 0,
            ..OagwConfig::default()
        };
        assert_eq!(
            config.validate().unwrap_err(),
            "token_cache_capacity must be greater than 0"
        );
    }

    #[test]
    fn ssrf_policy_defaults_to_enabled() {
        let policy: SsrfPolicyConfig = serde_json::from_value(json!({})).unwrap();
        assert!(policy.enabled);
    }
}
