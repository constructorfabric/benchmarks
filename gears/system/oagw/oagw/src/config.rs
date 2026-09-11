// Created: 2026-09-01 by Constructor Tech
//! Gear-level configuration.
//!
//! Loaded from the `gears.oagw.config` YAML section. See
//! `docs/ADR/0008-oauth2-client-credentials-auth-plugin.md` for the token
//! cache keys and `docs/DESIGN.md` §3.2 for the timeout semantics.
//!
//! ```yaml
//! gears:
//!   oagw:
//!     config:
//!       proxy_timeout_secs: 2
//!       allow_http_upstream: true
//!       ssrf_policy:
//!         enabled: false
//! ```

use serde::{Deserialize, Serialize};

/// `allow_http_upstream` does not widen what the API accepts — see
/// [`docs/DESIGN.md`](../../../docs/DESIGN.md) §3.2 — it only decides whether
/// a plaintext connection is actually established.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
pub struct SsrfPolicy {
    /// Deny upstreams whose endpoints resolve to loopback, link-local,
    /// RFC1918 or other private ranges. The E2E suite disables this so
    /// local mocks can act as upstreams.
    pub enabled: bool,
    /// Endpoints always allowed, even when `enabled` is true.
    pub allowed_cidrs: Vec<String>,
}

/// Circuit breaker thresholds, applied per upstream endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct CircuitBreakerConfig {
    /// Consecutive failures within [`Self::window_secs`] that trip the breaker.
    pub failure_threshold: u32,
    /// Rolling window in which failures are counted.
    pub window_secs: u64,
    /// How long an open breaker stays open before allowing a probe through.
    pub open_secs: u64,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            window_secs: 30,
            open_secs: 30,
        }
    }
}

/// Root configuration for the `oagw` gear.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct OagwConfig {
    /// Wall-clock budget for establishing the upstream connection and
    /// receiving the response head. Once the head arrives the body streams
    /// unbounded, so SSE and other long-lived responses are not cut off
    /// mid-stream.
    pub proxy_timeout_secs: u64,
    /// Permit `http://` upstream endpoints. The endpoint *scheme* field
    /// accepts plaintext regardless; this flag governs whether such a
    /// connection is actually made.
    pub allow_http_upstream: bool,
    /// Outbound address filtering.
    pub ssrf_policy: SsrfPolicy,
    /// Ceiling for cached OAuth2 access-token TTL (see ADR-0008).
    pub token_cache_ttl_secs: u64,
    /// Maximum entries in the OAuth2 token cache (see ADR-0008).
    pub token_cache_capacity: usize,
    /// Hard cap on proxied request bodies. `docs/DESIGN.md` §3.2 fixes the
    /// default at 100 MiB; requests are rejected before buffering.
    pub max_body_size_bytes: usize,
    /// Circuit breaker thresholds.
    pub circuit_breaker: CircuitBreakerConfig,
    /// Idle timeout applied between WebSocket frames.
    pub ws_idle_timeout_secs: u64,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: 30,
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicy::default(),
            token_cache_ttl_secs: 300,
            token_cache_capacity: 10_000,
            max_body_size_bytes: 100 * 1024 * 1024,
            circuit_breaker: CircuitBreakerConfig::default(),
            ws_idle_timeout_secs: 300,
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_design() {
        let cfg = OagwConfig::default();
        assert_eq!(cfg.proxy_timeout_secs, 30);
        assert!(!cfg.allow_http_upstream);
        assert_eq!(cfg.token_cache_ttl_secs, 300);
        assert_eq!(cfg.token_cache_capacity, 10_000);
        assert_eq!(cfg.max_body_size_bytes, 100 * 1024 * 1024);
    }

    #[test]
    fn deserializes_e2e_shape() {
        let json = serde_json::json!({
            "proxy_timeout_secs": 2,
            "allow_http_upstream": true,
            "ssrf_policy": { "enabled": false }
        });
        let cfg: OagwConfig = serde_json::from_value(json).expect("parses");
        assert_eq!(cfg.proxy_timeout_secs, 2);
        assert!(cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.enabled);
        // Unspecified keys fall back to defaults rather than zero.
        assert_eq!(cfg.token_cache_ttl_secs, 300);
        assert_eq!(cfg.circuit_breaker.failure_threshold, 5);
    }

    #[test]
    fn rejects_unknown_keys() {
        let json = serde_json::json!({ "proxy_timeout_secs": 2, "bogus": 1 });
        assert!(serde_json::from_value::<OagwConfig>(json).is_err());
    }
}
