//! Runtime configuration for the OAGW gear.
//!
//! Read from the `gears.oagw.config` section of the host config file.
//! The e2e-local configuration ships:
//!
//! ```yaml
//! oagw:
//!   config:
//!     proxy_timeout_secs: 2
//!     allow_http_upstream: true
//!     ssrf_policy:
//!       enabled: false
//! ```
//!
//! All fields have safe defaults so the gear bootstraps even when the
//! section is absent entirely.

use serde::{Deserialize, Serialize};

/// SSRF protection policy.
///
/// When enabled, upstream resolution refuses hosts that resolve to
/// link-local / loopback / private address space (unless an explicit
/// allowlist carve-out is present). The e2e-local configuration disables
/// it so acceptance tests may proxy to `127.0.0.1` mock servers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SsrfPolicy {
    /// Master switch for SSRF host screening.
    pub enabled: bool,
    /// Hostnames / IPs permanently exempt from screening.
    pub allowlist: Vec<String>,
}

impl Default for SsrfPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            allowlist: Vec::new(),
        }
    }
}

/// OAGW gear configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OagwConfig {
    /// Per-request timeout for upstream calls, in seconds. `0` disables
    /// the timeout (not recommended).
    pub proxy_timeout_secs: u64,
    /// Permit `http://` upstream endpoints (development / test only;
    /// production deployments must keep this `false`).
    pub allow_http_upstream: bool,
    /// SSRF host screening policy for upstream endpoints.
    pub ssrf_policy: SsrfPolicy,
    /// TTL for cached `OAuth2` client-credentials tokens, in seconds.
    /// Per ADR-0008, fetched tokens are cached to avoid a token-endpoint
    /// round-trip on every proxied request.
    pub token_cache_ttl_secs: u64,
    /// Capacity of the `OAuth2` token cache (distinct tokens cached).
    pub token_cache_capacity: usize,
    /// Maximum accepted upstream response body size in bytes. Larger
    /// bodies are truncated with `413 Payload Too Large`.
    pub max_response_body_bytes: u64,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: 30,
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicy::default(),
            token_cache_ttl_secs: 300,
            token_cache_capacity: 10_000,
            max_response_body_bytes: 100 * 1024 * 1024,
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn parses_e2e_local_config_section() {
        let raw = serde_json::json!({
            "proxy_timeout_secs": 2,
            "allow_http_upstream": true,
            "ssrf_policy": { "enabled": false }
        });
        let cfg: OagwConfig = serde_json::from_value(raw).expect("parse e2e config");
        assert_eq!(cfg.proxy_timeout_secs, 2);
        assert!(cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.enabled);
        // Fields absent from the e2e config pick up defaults.
        assert_eq!(cfg.token_cache_ttl_secs, 300);
        assert_eq!(cfg.max_response_body_bytes, 100 * 1024 * 1024);
    }

    #[test]
    fn defaults_when_section_absent() {
        let cfg = OagwConfig::default();
        assert!(!cfg.allow_http_upstream);
        assert!(cfg.ssrf_policy.enabled);
    }

    #[test]
    fn rejects_unknown_fields() {
        let raw = serde_json::json!({ "bogus": true });
        assert!(serde_json::from_value::<OagwConfig>(raw).is_err());
    }
}
