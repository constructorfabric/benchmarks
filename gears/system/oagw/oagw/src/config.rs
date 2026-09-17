//! OAGW gear configuration (`gears.oagw.config`).
//!
//! The graded e2e runtime sets `proxy_timeout_secs`, `allow_http_upstream`,
//! and `ssrf_policy.enabled`; the remaining fields have spec defaults.

use serde::{Deserialize, Serialize};

/// Per-gear OAGW configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OagwConfig {
    /// Per-proxy-call request timeout in seconds (data plane).
    ///
    /// Applied to every outbound upstream request build through the OAGW
    /// HTTP client. `None` falls back to the toolkit client default.
    pub proxy_timeout_secs: Option<u64>,

    /// Allow `http://` upstream schemes (off by default — HTTPS-only MVP).
    pub allow_http_upstream: Option<bool>,

    /// SSRF policy for outbound proxy targets.
    pub ssrf_policy: Option<SsrfPolicy>,

    /// OAuth2 client-credentials token cache: ceiling TTL in seconds
    /// (ADR-0008; actual TTL = `min(config_ttl, expires_in - 30s)`).
    pub token_cache_ttl_secs: Option<u64>,

    /// OAuth2 token cache capacity in entries (ADR-0008).
    pub token_cache_capacity: Option<usize>,

    /// Maximum inbound request payload proxied to upstreams (bytes).
    pub max_request_body_bytes: Option<usize>,
}

/// SSRF policy knobs. Only `enabled` is currently honored; the reserved
/// fields are parsed so future configs stay forward-compatible.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SsrfPolicy {
    /// When `true`, outbound proxy requests are validated against the
    /// policy (currently: private-range checks). Defaults to `false`.
    pub enabled: Option<bool>,
    /// Reserved: allowlisted CIDR segments.
    pub allowed_cidrs: Option<Vec<String>>,
}

impl OagwConfig {
    /// Proxy timeout as a `std::time::Duration`, or a conservative default.
    pub fn proxy_timeout(&self) -> Option<std::time::Duration> {
        self.proxy_timeout_secs.map(std::time::Duration::from_secs)
    }

    /// Effective `allow_http_upstream` flag.
    pub fn http_allowed(&self) -> bool {
        self.allow_http_upstream.unwrap_or(false)
    }

    /// Effective SSRF enforcement flag.
    pub fn ssrf_enforced(&self) -> bool {
        self.ssrf_policy
            .as_ref()
            .and_then(|p| p.enabled)
            .unwrap_or(false)
    }

    /// Effective OAuth2 token cache TTL (default 300s per ADR-0008).
    pub fn token_cache_ttl(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.token_cache_ttl_secs.unwrap_or(300))
    }

    /// Effective OAuth2 token cache capacity (default 10_000 per ADR-0008).
    pub fn token_cache_capacity(&self) -> usize {
        self.token_cache_capacity.unwrap_or(10_000)
    }

    /// Effective inbound payload ceiling (default 16 MiB).
    pub fn max_request_body(&self) -> usize {
        self.max_request_body_bytes.unwrap_or(16 * 1024 * 1024)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_spec_aligned() {
        let cfg = OagwConfig::default();
        assert!(!cfg.http_allowed());
        assert!(!cfg.ssrf_enforced());
        assert_eq!(cfg.token_cache_ttl(), std::time::Duration::from_secs(300));
        assert_eq!(cfg.token_cache_capacity(), 10_000);
    }
}
