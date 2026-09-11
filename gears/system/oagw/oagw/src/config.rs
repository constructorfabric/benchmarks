//! Configuration for the OAGW gear.
//!
//! Values come from `gears.oagw.config` in the deployment configuration and are
//! read with [`toolkit::GearCtx::config_or_default`] so that a deployment which
//! does not configure the gear still boots with the documented defaults.

use serde::Deserialize;

/// Server-sent-request-forgery posture.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct SsrfPolicy {
    /// When `true`, endpoint hosts are screened against private, loopback and
    /// link-local ranges before an upstream is created and before it is
    /// dialled. The default posture is off, because the gateway is mounted
    /// inside a platform whose upstreams are frequently local.
    pub enabled: bool,
}

/// `OAuth2` client-credentials token cache settings (ADR 0008).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TokenCacheConfig {
    /// Maximum number of cached access tokens.
    pub max_entries: u64,
    /// Lower bound applied to every upstream-provided `expires_in`, so a token
    /// is never used after its documented safety margin.
    pub min_ttl_secs: u64,
}

impl Default for TokenCacheConfig {
    fn default() -> Self {
        Self {
            max_entries: 1_000,
            min_ttl_secs: 30,
        }
    }
}

/// Gear configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OagwConfig {
    /// Default upstream call timeout in seconds for routes that do not carry
    /// their own `timeout_secs`.
    pub proxy_timeout_secs: u64,

    /// Whether a plaintext upstream connection may be made. HTTPS-only is the
    /// documented default posture; only this flag lifts it, and it lifts only
    /// the transport decision — the endpoint model accepts `http` as a scheme
    /// regardless.
    pub allow_http_upstream: bool,

    /// SSRF screening policy.
    pub ssrf_policy: SsrfPolicy,

    /// `OAuth2` token cache policy.
    pub token_cache: TokenCacheConfig,

    /// Largest request body the gateway forwards, in bytes (DESIGN: 100 MB
    /// hard limit). A body beyond it is refused before a byte is read.
    pub max_body_bytes: u64,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: 30,
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicy::default(),
            token_cache: TokenCacheConfig::default(),
            max_body_bytes: 100 * 1024 * 1024,
        }
    }
}

impl OagwConfig {
    /// Validate the configuration.
    ///
    /// # Errors
    /// Returns an error when a value is outside its documented range.
    pub fn validate(&self) -> Result<(), anyhow::Error> {
        if self.proxy_timeout_secs == 0 {
            anyhow::bail!("proxy_timeout_secs must be greater than zero");
        }
        Ok(())
    }

    /// Default upstream call timeout.
    #[must_use]
    pub fn proxy_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.proxy_timeout_secs)
    }
}

#[cfg(test)]
mod config_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn defaults_match_documented_posture() {
        let cfg = OagwConfig::default();
        assert_eq!(cfg.proxy_timeout_secs, 30);
        assert!(
            !cfg.allow_http_upstream,
            "HTTPS-only is the default posture"
        );
        assert!(!cfg.ssrf_policy.enabled);
        assert_eq!(cfg.token_cache.max_entries, 1_000);
        assert_eq!(cfg.token_cache.min_ttl_secs, 30);
        assert_eq!(cfg.max_body_bytes, 100 * 1024 * 1024);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn deserializes_the_graded_configuration() {
        let raw = serde_json::json!({
            "proxy_timeout_secs": 2,
            "allow_http_upstream": true,
            "max_body_bytes": 16,
            "ssrf_policy": { "enabled": false }
        });
        let cfg: OagwConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(cfg.proxy_timeout_secs, 2);
        assert_eq!(cfg.max_body_bytes, 16);
        assert!(cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.enabled);
        assert_eq!(cfg.proxy_timeout(), std::time::Duration::from_secs(2));
    }

    #[test]
    fn rejects_unknown_fields_and_zero_timeout() {
        assert!(serde_json::from_value::<OagwConfig>(serde_json::json!({ "nope": 1 })).is_err());
        let cfg: OagwConfig =
            serde_json::from_value(serde_json::json!({ "proxy_timeout_secs": 0 })).unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn empty_document_yields_defaults() {
        let cfg: OagwConfig = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(cfg, OagwConfig::default());
    }
}
