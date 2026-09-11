//! Gear configuration, deserialized from the `oagw.config` YAML block.

use serde::Deserialize;

/// Hard ceiling on a proxied request body, before it is buffered.
pub const MAX_BODY_BYTES: usize = 100 * 1024 * 1024;

/// Default lifetime of a cached `OAuth2` token, in seconds.
pub const DEFAULT_TOKEN_CACHE_TTL_SECS: u64 = 300;

/// Default capacity of the `OAuth2` token cache.
pub const DEFAULT_TOKEN_CACHE_CAPACITY: usize = 10_000;

/// SSRF guard posture.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case", default)]
pub struct SsrfPolicy {
    /// Whether SSRF checks run at all.
    pub enabled: bool,
    /// Whether private/loopback/link-local addresses may be dialled.
    pub allow_private_addresses: bool,
    /// Explicit host allowlist; consulted only when `enabled` is true.
    pub allowlist: Vec<String>,
}

impl Default for SsrfPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            allow_private_addresses: false,
            allowlist: Vec::new(),
        }
    }
}

/// Gear configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case", default)]
pub struct OagwConfig {
    /// Hard cap on a proxied request body, in bytes. Checked before buffering.
    pub max_body_bytes: usize,
    /// Total budget for a proxy round trip, in seconds.
    pub proxy_timeout_secs: u64,
    /// Budget for establishing the upstream connection, in seconds.
    pub connect_timeout_secs: u64,
    /// Whether a plaintext (`http`/`ws`) connection may actually be made.
    ///
    /// This governs *connection*, not *acceptance*: an `http` endpoint scheme is
    /// always accepted at create time (see `domain::alias` and the wire
    /// contract), and this switch decides only whether the gateway will dial it.
    pub allow_http_upstream: bool,
    /// SSRF guard posture.
    pub ssrf_policy: SsrfPolicy,
    /// Optional cap on the size of one upstream endpoint pool.
    pub max_endpoints_per_upstream: Option<usize>,
    /// `OAuth2` token-cache entry lifetime, in seconds.
    pub token_cache_ttl_secs: u64,
    /// `OAuth2` token-cache capacity, in entries.
    pub token_cache_capacity: usize,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            max_body_bytes: MAX_BODY_BYTES,
            proxy_timeout_secs: 60,
            connect_timeout_secs: 10,
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicy::default(),
            max_endpoints_per_upstream: None,
            token_cache_ttl_secs: DEFAULT_TOKEN_CACHE_TTL_SECS,
            token_cache_capacity: DEFAULT_TOKEN_CACHE_CAPACITY,
        }
    }
}

impl OagwConfig {
    /// Validates the configuration.
    ///
    /// # Errors
    /// Returns an error when a numeric bound is out of range.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.max_body_bytes == 0 || self.max_body_bytes > MAX_BODY_BYTES {
            return Err(anyhow::anyhow!(
                "oagw.config.max_body_bytes must be between 1 and {MAX_BODY_BYTES}"
            ));
        }
        if self.proxy_timeout_secs == 0 {
            return Err(anyhow::anyhow!("oagw.config.proxy_timeout_secs must be > 0"));
        }
        if self.connect_timeout_secs == 0 {
            return Err(anyhow::anyhow!(
                "oagw.config.connect_timeout_secs must be > 0"
            ));
        }
        if let Some(max) = self.max_endpoints_per_upstream
            && max == 0
        {
            return Err(anyhow::anyhow!(
                "oagw.config.max_endpoints_per_upstream must be > 0"
            ));
        }
        if self.token_cache_ttl_secs == 0 {
            return Err(anyhow::anyhow!("oagw.config.token_cache_ttl_secs must be > 0"));
        }
        if self.token_cache_capacity == 0 {
            return Err(anyhow::anyhow!(
                "oagw.config.token_cache_capacity must be > 0"
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let config = OagwConfig::default();
        assert_eq!(config.max_body_bytes, MAX_BODY_BYTES);
        assert!(!config.allow_http_upstream);
        assert!(config.ssrf_policy.enabled);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn e2e_shape_parses() {
        let raw = serde_json::json!({
            "proxy_timeout_secs": 2,
            "allow_http_upstream": true,
            "ssrf_policy": { "enabled": false }
        });
        let config: OagwConfig = serde_json::from_value(raw).expect("valid config");
        assert_eq!(config.proxy_timeout_secs, 2);
        assert!(config.allow_http_upstream);
        assert!(!config.ssrf_policy.enabled);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn zero_timeout_is_rejected() {
        let config = OagwConfig {
            proxy_timeout_secs: 0,
            ..OagwConfig::default()
        };
        assert!(config.validate().is_err());
    }
}
