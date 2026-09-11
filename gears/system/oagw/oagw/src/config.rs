//! Gear-level configuration (`gears.oagw.config` in the server config).

use serde::Deserialize;

/// Policy for outbound requests to private address ranges (DESIGN `cpt-cf-oagw-constraint-ssrf`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
#[derive(Default)]
pub struct SsrfPolicy {
    /// Enable the private-address checks.
    pub enabled: bool,
    /// Address blocks the gateway refuses to reach, in CIDR notation.
    #[serde(default)]
    pub denied_cidrs: Vec<String>,
}


/// Gear configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct OagwConfig {
    /// Upstream call timeout in seconds (PRD §5.2).
    pub proxy_timeout_secs: u64,
    /// Legalises `scheme: "http"` upstream endpoints (DESIGN `cpt-cf-oagw-constraint-https-only`).
    pub allow_http_upstream: bool,
    /// Outbound SSRF policy.
    pub ssrf_policy: SsrfPolicy,
    /// Ceiling for cached OAuth2 access tokens (ADR-0008).
    pub token_cache_ttl_secs: u64,
    /// Maximum entries in the OAuth2 token cache (ADR-0008).
    pub token_cache_capacity: usize,
    /// Maximum buffered request body, in bytes.
    pub max_body_bytes: usize,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: 30,
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicy::default(),
            token_cache_ttl_secs: 300,
            token_cache_capacity: 10_000,
            max_body_bytes: crate::infra::proxy::service::MAX_BODY_BYTES,
        }
    }
}

impl OagwConfig {
    /// Reject configuration the gear cannot honour.
    pub fn validate(&self) -> Result<(), crate::domain::error::DomainError> {
        if self.proxy_timeout_secs == 0 {
            return Err(crate::domain::error::DomainError::Validation(
                "proxy_timeout_secs must be at least 1".to_string(),
            ));
        }
        if self.proxy_timeout_secs > 600 {
            return Err(crate::domain::error::DomainError::Validation(
                "proxy_timeout_secs must be at most 600".to_string(),
            ));
        }
        if self.token_cache_ttl_secs == 0 {
            return Err(crate::domain::error::DomainError::Validation(
                "token_cache_ttl_secs must be at least 1".to_string(),
            ));
        }
        if self.token_cache_capacity == 0 {
            return Err(crate::domain::error::DomainError::Validation(
                "token_cache_capacity must be at least 1".to_string(),
            ));
        }
        if self.max_body_bytes == 0 || self.max_body_bytes > 1024 * 1024 * 1024 {
            return Err(crate::domain::error::DomainError::Validation(
                "max_body_bytes must be between 1 and 1GB".to_string(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
