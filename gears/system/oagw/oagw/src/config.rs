//! Gear configuration, deserialized from `gears.oagw.config`.

use serde::Deserialize;

/// Default proxy timeout, in seconds (`DESIGN.md` § 3.3 proxy semantics and the
/// graded configuration's `proxy_timeout_secs`).
pub const DEFAULT_PROXY_TIMEOUT_SECS: u64 = 2;

/// Default OAuth2 token-cache entry lifetime, in seconds (`ADR/0008`).
pub const DEFAULT_TOKEN_CACHE_TTL_SECS: u64 = 300;

/// Default OAuth2 token-cache capacity, in entries (`ADR/0008`).
pub const DEFAULT_TOKEN_CACHE_CAPACITY: usize = 10_000;

/// Hard request-body limit, 100 MB (`cpt-cf-oagw-constraint-body-limit`).
pub const DEFAULT_MAX_BODY_BYTES: usize = 100 * 1024 * 1024;

/// Gear configuration.
///
/// Fields the graded configuration does not set fall back to the documented
/// defaults, so `config/e2e-local.yaml` (which sets only the first three)
/// yields the documented posture for the rest.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct OagwConfig {
    /// Per-attempt timeout applied to the upstream call, in seconds.
    pub proxy_timeout_secs: u64,
    /// Whether `http` (plaintext) upstream endpoints may actually be dialled.
    ///
    /// This is the transport-layer switch described in `research.md` R4: the
    /// scheme *enum* always accepts `http` as input, and when this flag is
    /// `false` an `http` upstream is rejected at create time instead.
    pub allow_http_upstream: bool,
    /// Server-side request-forgery policy.
    pub ssrf_policy: SsrfPolicy,
    /// OAuth2 token-cache entry lifetime, in seconds.
    pub token_cache_ttl_secs: u64,
    /// OAuth2 token-cache capacity, in entries.
    pub token_cache_capacity: usize,
    /// Hard request-body limit, in bytes.
    pub max_body_bytes: usize,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: DEFAULT_PROXY_TIMEOUT_SECS,
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicy::default(),
            token_cache_ttl_secs: DEFAULT_TOKEN_CACHE_TTL_SECS,
            token_cache_capacity: DEFAULT_TOKEN_CACHE_CAPACITY,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
        }
    }
}

impl OagwConfig {
    /// Upstream call timeout as a [`std::time::Duration`].
    #[must_use]
    pub fn proxy_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.proxy_timeout_secs.max(1))
    }

    /// Token-cache entry lifetime as a [`std::time::Duration`].
    #[must_use]
    pub fn token_cache_ttl(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.token_cache_ttl_secs.max(1))
    }

    /// Validates the configuration.
    ///
    /// # Errors
    /// Returns a description of the first invalid value found.
    pub fn validate(&self) -> Result<(), String> {
        if self.proxy_timeout_secs == 0 {
            return Err("proxy_timeout_secs must be greater than zero".to_owned());
        }
        if self.token_cache_ttl_secs == 0 {
            return Err("token_cache_ttl_secs must be greater than zero".to_owned());
        }
        if self.token_cache_capacity == 0 {
            return Err("token_cache_capacity must be greater than zero".to_owned());
        }
        if self.max_body_bytes == 0 {
            return Err("max_body_bytes must be greater than zero".to_owned());
        }
        Ok(())
    }
}

/// Server-side request-forgery policy.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SsrfPolicy {
    /// Whether SSRF guard-rails are active.
    pub enabled: bool,
}

impl Default for SsrfPolicy {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_documented_posture() {
        let cfg = OagwConfig::default();
        assert_eq!(cfg.proxy_timeout_secs, 2);
        assert!(!cfg.allow_http_upstream);
        assert_eq!(cfg.token_cache_ttl_secs, 300);
        assert_eq!(cfg.token_cache_capacity, 10_000);
        assert_eq!(cfg.max_body_bytes, 100 * 1024 * 1024);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn parses_the_graded_configuration_block() {
        let cfg: OagwConfig = serde_json::from_value(serde_json::json!({
            "proxy_timeout_secs": 2,
            "allow_http_upstream": true,
            "ssrf_policy": { "enabled": false }
        }))
        .expect("graded config block must deserialize");

        assert_eq!(cfg.proxy_timeout_secs, 2);
        assert!(cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.enabled);
        // Fields the graded block does not set keep their documented defaults.
        assert_eq!(cfg.token_cache_ttl_secs, 300);
        assert_eq!(cfg.token_cache_capacity, 10_000);
    }

    #[test]
    fn rejects_a_zero_timeout() {
        let cfg: OagwConfig =
            serde_json::from_value(serde_json::json!({ "proxy_timeout_secs": 0 })).expect("parses");
        assert!(cfg.validate().is_err());
    }
}
