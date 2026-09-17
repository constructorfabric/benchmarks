//! Configuration for the `oagw` (outbound API gateway) gear.
//!
//! Every field is optional on the wire (`#[serde(default)]` on the container plus
//! per-field defaults) so a deployment that only sets
//! `proxy_timeout_secs: 2` still boots with the documented defaults.
//!
//! The control plane (this part of the crate) only needs the *management*
//! settings; the data plane (part 2) reads the proxy/SSRF/token-cache settings
//! from the same struct so operators configure the gear in one place.

use serde::Deserialize;

/// Default `$top` for management list endpoints (DESIGN.md "management API").
const DEFAULT_LIST_TOP: u64 = 50;
/// Maximum `$top` for management list endpoints.
const MAX_LIST_TOP: u64 = 100;

/// Default token-cache TTL (ADR 0008).
const DEFAULT_TOKEN_CACHE_TTL_SECS: u64 = 300;
/// Default token-cache capacity (ADR 0008).
const DEFAULT_TOKEN_CACHE_CAPACITY: u64 = 10_000;

/// Root configuration for the `oagw` gear.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OagwConfig {
    /// Total wall-clock budget for a proxied request, including plugin
    /// execution and the upstream exchange.
    pub proxy_timeout_secs: u64,
    /// Whether a plaintext (`http`) upstream connection may actually be made.
    ///
    /// This flag **never** gates request/schema validation: `http` is a legal
    /// endpoint scheme unconditionally. It only governs whether the data plane
    /// will *dial* a cleartext connection at proxy time (part 2).
    pub allow_http_upstream: bool,
    /// Outbound SSRF guard policy (dial-time, data plane).
    pub ssrf_policy: SsrfPolicyConfig,
    /// OAuth2 client-credentials token cache (ADR 0008).
    pub token_cache: TokenCacheConfig,
    /// Largest request body the data plane will buffer, in bytes.
    pub body_limit_bytes: u64,
    /// How long an evicted plugin/config entry stays parked before it is
    /// garbage collected (data-plane cache bookkeeping).
    pub plugin_gc_ttl_secs: u64,
    /// Capacity of the data-plane resolved-config cache.
    pub dp_cache_capacity: u64,
    /// Capacity of the control-plane resolved-config cache.
    pub cp_cache_capacity: u64,
    /// TTL of the per-upstream HTTP-version cache (data plane).
    pub http_version_cache_ttl_secs: u64,
    /// Management API list pagination bounds.
    pub management: ManagementConfig,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: 30,
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicyConfig::default(),
            token_cache: TokenCacheConfig::default(),
            body_limit_bytes: 100 * 1024 * 1024,
            plugin_gc_ttl_secs: 30 * 24 * 3600,
            dp_cache_capacity: 1_000,
            cp_cache_capacity: 10_000,
            http_version_cache_ttl_secs: 3_600,
            management: ManagementConfig::default(),
        }
    }
}

/// Outbound SSRF guard policy.
///
/// The guard is a *dial-time* concern (data plane, part 2) but its shape is
/// declared here so configuration is stable across both parts.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SsrfPolicyConfig {
    /// Whether outbound dials are screened against the allow/deny lists.
    pub enabled: bool,
    /// Hosts / CIDRs that are always allowed (checked before the deny list).
    pub allowed_hosts: Vec<String>,
    /// Hosts / CIDRs that are always refused.
    pub denied_hosts: Vec<String>,
}

impl Default for SsrfPolicyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allowed_hosts: Vec::new(),
            denied_hosts: Vec::new(),
        }
    }
}

/// OAuth2 client-credentials token cache configuration (ADR 0008).
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TokenCacheConfig {
    /// Wall-clock TTL applied when the issuer does not advertise `expires_in`,
    /// and the upper bound when it does (the effective TTL is
    /// `min(self.ttl, expires_in - 30s)`).
    pub ttl_secs: u64,
    /// Maximum number of cached tokens (LRU eviction beyond that).
    pub capacity: u64,
}

impl Default for TokenCacheConfig {
    fn default() -> Self {
        Self {
            ttl_secs: DEFAULT_TOKEN_CACHE_TTL_SECS,
            capacity: DEFAULT_TOKEN_CACHE_CAPACITY,
        }
    }
}

impl TokenCacheConfig {
    /// Cache TTL as a [`std::time::Duration`].
    #[must_use]
    pub fn ttl(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.ttl_secs)
    }

    /// Cache capacity as a `usize` (clamped to at least 1).
    #[must_use]
    pub fn capacity(&self) -> usize {
        usize::try_from(self.capacity).unwrap_or(usize::MAX).max(1)
    }
}

/// Management API pagination bounds.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ManagementConfig {
    /// Default page size for list endpoints.
    pub default_top: u64,
    /// Maximum page size for list endpoints.
    pub max_top: u64,
}

impl Default for ManagementConfig {
    fn default() -> Self {
        Self {
            default_top: DEFAULT_LIST_TOP,
            max_top: MAX_LIST_TOP,
        }
    }
}

impl OagwConfig {
    /// # Errors
    /// Returns `Err` with a human-readable description of the first invalid
    /// field.
    pub fn validate(&self) -> Result<(), String> {
        if self.proxy_timeout_secs == 0 {
            return Err("proxy_timeout_secs must be > 0".to_owned());
        }
        if self.body_limit_bytes == 0 {
            return Err("body_limit_bytes must be > 0".to_owned());
        }
        if self.token_cache.ttl_secs == 0 {
            return Err("token_cache.ttl_secs must be > 0".to_owned());
        }
        if self.token_cache.capacity == 0 {
            return Err("token_cache.capacity must be > 0".to_owned());
        }
        if self.plugin_gc_ttl_secs == 0 {
            return Err("plugin_gc_ttl_secs must be > 0".to_owned());
        }
        if self.dp_cache_capacity == 0 {
            return Err("dp_cache_capacity must be > 0".to_owned());
        }
        if self.cp_cache_capacity == 0 {
            return Err("cp_cache_capacity must be > 0".to_owned());
        }
        if self.http_version_cache_ttl_secs == 0 {
            return Err("http_version_cache_ttl_secs must be > 0".to_owned());
        }
        if self.management.default_top == 0 {
            return Err("management.default_top must be > 0".to_owned());
        }
        if self.management.max_top == 0 {
            return Err("management.max_top must be > 0".to_owned());
        }
        if self.management.default_top > self.management.max_top {
            return Err("management.default_top must be <= management.max_top".to_owned());
        }
        Ok(())
    }

    /// Clamped `$top` for a management list request.
    #[must_use]
    pub fn clamp_top(&self, requested: Option<u64>) -> u64 {
        match requested {
            Some(0) | None => self.management.default_top,
            Some(v) => v.min(self.management.max_top).max(1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::OagwConfig;

    #[test]
    fn defaults_are_valid() {
        let cfg = OagwConfig::default();
        assert_eq!(cfg.proxy_timeout_secs, 30);
        assert!(!cfg.allow_http_upstream);
        assert!(cfg.ssrf_policy.enabled);
        assert_eq!(cfg.token_cache.ttl_secs, 300);
        assert_eq!(cfg.token_cache.capacity, 10_000);
        assert_eq!(cfg.management.default_top, 50);
        assert_eq!(cfg.management.max_top, 100);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn partial_config_fills_defaults() {
        let cfg: OagwConfig =
            serde_json::from_str(r#"{"proxy_timeout_secs": 2, "allow_http_upstream": true}"#)
                .expect("deserialize");
        assert_eq!(cfg.proxy_timeout_secs, 2);
        assert!(cfg.allow_http_upstream);
        assert_eq!(cfg.token_cache.ttl_secs, 300);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let err = serde_json::from_str::<OagwConfig>(r#"{"nope": 1}"#);
        assert!(err.is_err());
    }

    #[test]
    fn zero_values_are_invalid() {
        let cfg: OagwConfig = serde_json::from_str(r#"{"proxy_timeout_secs": 0}"#).expect("parse");
        assert!(cfg.validate().is_err());
    }
}
