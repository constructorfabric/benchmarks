//! `OagwConfig` — the `oagw.config` gear configuration block
//! (`cpt-cf-oagw-dod-gear-foundation-config-model`).
//!
//! Defaults are the FEATURE's verbatim defaults. Every key is validated at
//! init and the gear fails fast on an invalid block; an unknown key is
//! rejected (`deny_unknown_fields`), as is a credential-bearing field
//! carrying anything other than a `cred://` reference
//! (`cpt-cf-oagw-dod-gear-foundation-credential-boundary`).

use serde::{Deserialize, Serialize};

pub use crate::domain::credential::reject_non_cred_reference_values;

pub use crate::domain::dto::{AuthConfig, HeadersConfig};
use crate::domain::error::DomainError;

/// The 100 MB hard body-size ceiling. A configured limit may not exceed it;
/// a declared or observed body over the limit is `413` before buffering.
pub const MAX_BODY_SIZE_CEILING: u64 = 100 * 1024 * 1024;

/// `ssrf_policy` — the SSRF posture the proxy applies to a resolved target.
///
/// Only `disabled` exists in the graded configuration: the target set is the
/// operator-declared endpoint pool, so there is no user-controlled host to
/// guard.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SsrfPolicy {
    /// No SSRF guard. The target comes from the declared endpoint pool.
    #[default]
    Disabled,
}

/// The OAuth2 token cache settings (ADR 0008) — a
/// `pingora_memory_cache::MemoryCache` in the data plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenCacheConfig {
    /// Entry time-to-live in seconds, default `300`.
    pub ttl_secs: u64,
    /// Maximum number of cached entries, default `10_000`.
    pub capacity: usize,
}

impl Default for TokenCacheConfig {
    fn default() -> Self {
        Self { ttl_secs: DEFAULT_TOKEN_CACHE_TTL_SECS, capacity: DEFAULT_TOKEN_CACHE_CAPACITY }
    }
}

/// Default `token_cache_ttl_secs`.
pub const DEFAULT_TOKEN_CACHE_TTL_SECS: u64 = 300;
/// Default `token_cache_capacity`.
pub const DEFAULT_TOKEN_CACHE_CAPACITY: usize = 10_000;
/// Default body-size limit in bytes (the 100 MB ceiling itself).
pub const DEFAULT_MAX_BODY_SIZE_BYTES: u64 = MAX_BODY_SIZE_CEILING;
/// Default `proxy_timeout_secs`.
pub const DEFAULT_PROXY_TIMEOUT_SECS: u64 = 2;

/// The `oagw.config` block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OagwConfig {
    /// `http` is a legal upstream scheme only while this is true (graded
    /// deviation 2, lifting `cpt-cf-oagw-constraint-https-only`).
    pub allow_http_upstream: bool,
    /// OAuth2 token cache entry TTL in seconds.
    pub token_cache_ttl_secs: u64,
    /// OAuth2 token cache entry capacity.
    pub token_cache_capacity: usize,
    /// SSRF posture; only `disabled` exists.
    pub ssrf_policy: SsrfPolicy,
    /// Request body limit in bytes, `<=` [`MAX_BODY_SIZE_CEILING`].
    pub max_body_size_bytes: u64,
    /// Proxy timeout in seconds, bounding connection establishment and the
    /// complete buffered request/response exchange.
    pub proxy_timeout_secs: u64,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            allow_http_upstream: false,
            token_cache_ttl_secs: DEFAULT_TOKEN_CACHE_TTL_SECS,
            token_cache_capacity: DEFAULT_TOKEN_CACHE_CAPACITY,
            ssrf_policy: SsrfPolicy::Disabled,
            max_body_size_bytes: DEFAULT_MAX_BODY_SIZE_BYTES,
            proxy_timeout_secs: DEFAULT_PROXY_TIMEOUT_SECS,
        }
    }
}

impl OagwConfig {
    /// The `TokenCacheConfig` this block projects.
    #[must_use]
    pub const fn token_cache_config(&self) -> TokenCacheConfig {
        TokenCacheConfig { ttl_secs: self.token_cache_ttl_secs, capacity: self.token_cache_capacity }
    }

    /// Validate every documented invariant.
    ///
    /// # Errors
    ///
    /// Returns a [`DomainError::ValidationError`] naming the offending key.
    // @cpt-begin:cpt-cf-oagw-flow-gear-foundation-config-load:p1:inst-gf-cfg-3
    pub fn validate(&self) -> Result<(), DomainError> {
        if self.proxy_timeout_secs == 0 {
            return Err(DomainError::ValidationError {
                detail: "proxy_timeout_secs must be a positive number of seconds".to_owned(),
                path: Some("proxy_timeout_secs".to_owned()),
                trace_id: None,
            });
        }
        if self.token_cache_ttl_secs == 0 {
            return Err(DomainError::ValidationError {
                detail: "token_cache_ttl_secs must be a positive number of seconds".to_owned(),
                path: Some("token_cache_ttl_secs".to_owned()),
                trace_id: None,
            });
        }
        if self.token_cache_capacity == 0 {
            return Err(DomainError::ValidationError {
                detail: "token_cache_capacity must be a positive number of entries".to_owned(),
                path: Some("token_cache_capacity".to_owned()),
                trace_id: None,
            });
        }
        if self.max_body_size_bytes == 0 {
            return Err(DomainError::ValidationError {
                detail: "max_body_size_bytes must be a positive number of bytes".to_owned(),
                path: Some("max_body_size_bytes".to_owned()),
                trace_id: None,
            });
        }
        if self.max_body_size_bytes > MAX_BODY_SIZE_CEILING {
            return Err(DomainError::ValidationError {
                detail: "max_body_size_bytes must not exceed the 100 MB ceiling".to_owned(),
                path: Some("max_body_size_bytes".to_owned()),
                trace_id: None,
            });
        }
        Ok(())
        // @cpt-end:cpt-cf-oagw-flow-gear-foundation-config-load:p1:inst-gf-cfg-3
    }

    /// Deserialize the configuration from its JSON value and validate it
    /// (`inst-gf-cfg-1`, `inst-gf-cfg-2`, `inst-gf-cfg-9`).
    ///
    /// # Errors
    ///
    /// Returns the deserialization error (an unknown key is one) or the
    /// validation error.
    pub fn from_value(value: Option<&serde_json::Value>) -> Result<Self, DomainError> {
        match value {
            None | Some(serde_json::Value::Null) => {
                let config = Self::default();
                config.validate()?;
                Ok(config)
            }
            Some(value) => {
                let config: Self = serde_json::from_value(value.clone()).map_err(|error| {
                    DomainError::ValidationError {
                        detail: format!("invalid oagw.config: {error}"),
                        path: Some("oagw.config".to_owned()),
                        trace_id: None,
                    }
                })?;
                config.validate()?;
                Ok(config)
            }
        }
    }
}


// @cpt-begin:cpt-cf-oagw-flow-gear-foundation-config-load:p1:inst-gf-cfg-1
// @cpt-begin:cpt-cf-oagw-flow-gear-foundation-config-load:p1:inst-gf-cfg-2
// @cpt-begin:cpt-cf-oagw-flow-gear-foundation-config-load:p1:inst-gf-cfg-6
// @cpt-begin:cpt-cf-oagw-flow-gear-foundation-config-load:p1:inst-gf-cfg-7
// @cpt-begin:cpt-cf-oagw-flow-gear-foundation-config-load:p1:inst-gf-cfg-8
// @cpt-begin:cpt-cf-oagw-flow-gear-foundation-config-load:p1:inst-gf-cfg-9
#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
//
// @cpt-end:cpt-cf-oagw-flow-gear-foundation-config-load:p1:inst-gf-cfg-9
// @cpt-end:cpt-cf-oagw-flow-gear-foundation-config-load:p1:inst-gf-cfg-8
// @cpt-end:cpt-cf-oagw-flow-gear-foundation-config-load:p1:inst-gf-cfg-7
// @cpt-end:cpt-cf-oagw-flow-gear-foundation-config-load:p1:inst-gf-cfg-6
// @cpt-end:cpt-cf-oagw-flow-gear-foundation-config-load:p1:inst-gf-cfg-2
// @cpt-end:cpt-cf-oagw-flow-gear-foundation-config-load:p1:inst-gf-cfg-1
//
