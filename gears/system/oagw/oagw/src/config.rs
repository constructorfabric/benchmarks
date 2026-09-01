//! Deployment configuration for the `oagw` gear.
//!
//! Binds the `oagw.config` block of the deployment manifest. Every field has a
//! default so a gear can come up with an empty `config: {}` block; unknown keys
//! are refused so a typo in the manifest fails loudly instead of silently
//! falling back to a built-in default.

use serde::Deserialize;

/// Default page size for list endpoints (`DESIGN` §3.3, List Query Parameters).
pub const DEFAULT_PAGE_SIZE: u64 = 50;

/// Maximum accepted page size for list endpoints. Above it the request is
/// rejected with `400`, not silently clamped, so a client that asks for a
/// thousand rows learns its paging window is wrong.
pub const MAX_PAGE_SIZE: u64 = 100;

/// Server-side request-forgery posture for outbound calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SsrfPolicy {
    /// Enable DNS/IP allow-listing for outbound targets.
    pub enabled: bool,
}

impl Default for SsrfPolicy {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// `oagw.config` block.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OagwConfig {
    /// Per-attempt timeout applied to an outbound upstream call.
    pub proxy_timeout_secs: u64,
    /// Allow plaintext `http` upstreams. Production deployments must leave
    /// this `false` (`DESIGN` §2.2, HTTPS-only constraint).
    pub allow_http_upstream: bool,
    /// SSRF protection posture for outbound calls.
    pub ssrf_policy: SsrfPolicy,
    /// Default `$top` for list endpoints.
    pub list_default_page_size: u64,
    /// Maximum accepted `$top` for list endpoints.
    pub list_max_page_size: u64,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: 2,
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicy::default(),
            list_default_page_size: DEFAULT_PAGE_SIZE,
            list_max_page_size: MAX_PAGE_SIZE,
        }
    }
}

impl OagwConfig {
    /// # Errors
    /// Returns `Err` with a description of the first invalid field.
    pub fn validate(&self) -> Result<(), String> {
        if self.proxy_timeout_secs == 0 {
            return Err("proxy_timeout_secs must be > 0".to_owned());
        }
        if self.list_default_page_size == 0 {
            return Err("list_default_page_size must be > 0".to_owned());
        }
        if self.list_max_page_size < self.list_default_page_size {
            return Err("list_max_page_size must be >= list_default_page_size".to_owned());
        }
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "config_tests.rs"]
mod tests;
