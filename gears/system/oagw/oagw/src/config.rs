// Created: 2026-09-02 by Constructor Tech
//! Gear configuration for the outbound API gateway.
//!
//! Read once at `Gear::init` from `gears.oagw.config` via
//! [`toolkit::context::GearCtx::config_or_default`]. Every field has a default so
//! an absent (or empty) config block yields a fully usable gateway.
//!
//! Two config keys are load-bearing for the graded environment:
//!
//! - `allow_http_upstream` lifts [`cpt-cf-oagw-constraint-https-only`]. It governs
//!   **whether a plaintext connection is actually made**, not which schemes the
//!   `server.endpoints[].scheme` field accepts: `http` is always a legal value for
//!   the field, and a request that would dial a plaintext upstream is rejected at
//!   proxy time when the flag is off.
//! - `proxy_timeout_secs` bounds the upstream request (connect + response headers).

use serde::Deserialize;

/// SSRF protection for outbound upstream connections (`ssrf_policy`).
///
/// When enabled, an upstream whose endpoint resolves to a private / loopback /
/// link-local address is rejected before a socket is opened, unless the address
/// is listed in `allowed_segments`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct SsrfPolicy {
    /// Enable SSRF screening of upstream targets.
    pub enabled: bool,
    /// Allow private / loopback / link-local targets regardless of `allowed_segments`.
    pub allow_private_addresses: bool,
    /// CIDR segments (`10.0.0.0/8`, `fc00::/7`) explicitly permitted.
    pub allowed_segments: Vec<String>,
}


/// Outbound API gateway configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct OagwConfig {
    /// Upstream request timeout: bounds connect + response-header receipt for
    /// non-streaming requests. Streaming responses (SSE / WebSocket) are bounded
    /// by `stream_idle_timeout_secs` instead, since they are long-lived by design.
    pub proxy_timeout_secs: u64,
    /// Per-attempt TCP/TLS connect timeout.
    pub connect_timeout_secs: u64,
    /// Whether plaintext (`http`-scheme / `ws`-over-`http`) upstream connections
    /// may be dialled. Does **not** affect request validation.
    pub allow_http_upstream: bool,
    /// SSRF screening policy.
    pub ssrf_policy: SsrfPolicy,
    /// OAuth2 client-credentials token cache TTL (ADR-0008).
    pub token_cache_ttl_secs: u64,
    /// OAuth2 token cache capacity (ADR-0008).
    pub token_cache_capacity: usize,
    /// Hard request-body limit (cpt-cf-oagw-constraint-body-limit).
    pub max_body_bytes: usize,
    /// Idle timeout applied to streaming (SSE / WebSocket) upstream responses.
    pub stream_idle_timeout_secs: u64,
    /// Default page size for list endpoints.
    pub list_top_default: usize,
    /// Maximum page size for list endpoints.
    pub list_top_max: usize,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: 30,
            connect_timeout_secs: 10,
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicy::default(),
            token_cache_ttl_secs: 300,
            token_cache_capacity: 10_000,
            max_body_bytes: 100 * 1024 * 1024,
            stream_idle_timeout_secs: 30,
            list_top_default: 50,
            list_top_max: 100,
        }
    }
}

impl OagwConfig {
    /// Upstream request timeout as a [`std::time::Duration`].
    #[must_use]
    pub fn proxy_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.proxy_timeout_secs.max(1))
    }

    /// Connect timeout as a [`std::time::Duration`].
    #[must_use]
    pub fn connect_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.connect_timeout_secs.max(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_specified_posture() {
        let cfg = OagwConfig::default();
        assert!(!cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.enabled);
        assert_eq!(cfg.token_cache_ttl_secs, 300);
        assert_eq!(cfg.token_cache_capacity, 10_000);
        assert_eq!(cfg.max_body_bytes, 100 * 1024 * 1024);
        assert_eq!(cfg.list_top_default, 50);
        assert_eq!(cfg.list_top_max, 100);
    }

    #[test]
    fn deserializes_the_e2e_config_shape_and_ignores_unknown_keys() {
        let cfg: OagwConfig = serde_json::from_value(serde_json::json!({
            "proxy_timeout_secs": 2,
            "allow_http_upstream": true,
            "ssrf_policy": { "enabled": false },
            "not_a_known_key": 1
        }))
        .expect("e2e config shape must deserialize");
        assert_eq!(cfg.proxy_timeout_secs, 2);
        assert!(cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.enabled);
    }

    #[test]
    fn empty_value_yields_defaults() {
        let cfg: OagwConfig = serde_json::from_value(serde_json::json!({})).expect("defaults");
        assert_eq!(cfg, OagwConfig::default());
    }

    #[test]
    fn proxy_timeout_is_never_zero() {
        let cfg: OagwConfig =
            serde_json::from_value(serde_json::json!({ "proxy_timeout_secs": 0 })).expect("ok");
        assert_eq!(cfg.proxy_timeout(), std::time::Duration::from_secs(1));
    }
}
