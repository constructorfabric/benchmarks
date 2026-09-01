//! Gear configuration (`DESIGN` §3.2 `config.rs`, ADR-0008 token cache keys).
//!
//! Deserialized from the `gears.oagw.config` section of the server
//! configuration. Unknown keys are tolerated so operators can stage settings
//! that a given build does not consume.

use serde::{Deserialize, Serialize};

/// Default request-body hard limit (DESIGN constraint
/// `cpt-cf-oagw-constraint-body-limit`).
pub const DEFAULT_BODY_LIMIT_BYTES: u64 = 100 * 1024 * 1024;

/// Default upstream proxy timeout in seconds.
pub const DEFAULT_PROXY_TIMEOUT_SECS: u64 = 30;

/// Default upstream connect timeout in seconds.
pub const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 5;

/// Default stream idle timeout in seconds (applies to SSE/WebSocket relays).
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 300;

/// Default OAuth2 client-credentials token cache TTL (ADR-0008).
pub const DEFAULT_TOKEN_CACHE_TTL_SECS: u64 = 300;

/// Default OAuth2 client-credentials token cache capacity (ADR-0008).
pub const DEFAULT_TOKEN_CACHE_CAPACITY: usize = 10_000;

/// SSRF hardening switches (DESIGN §3.2 Security Considerations).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct SsrfPolicy {
    /// When `true`, SSRF checks (private/loopback/link-local segments and the
    /// configured deny/allow lists) are enforced before any upstream dial.
    pub enabled: bool,
    /// IP segments that are always allowed even when `enabled` is `true`.
    pub allowed_segments: Vec<String>,
    /// IP segments that are always rejected when `enabled` is `true`.
    pub blocked_segments: Vec<String>,
}

/// Server-side policy for plaintext upstream schemes.
///
/// The published wire contract (`schemas/upstream.v1.schema.json`) only lists
/// TLS-bearing schemes (`https`, `wss`, `wt`, `grpc`). The server additionally
/// accepts `http` and `ws` endpoint schemes **only** when this switch is on,
/// which is how the local/e2e configuration proxies to plaintext mock
/// upstreams (DESIGN constraint `cpt-cf-oagw-constraint-https-only`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
#[derive(Default)]
pub struct PlaintextUpstreamPolicy {
    /// Allow `http` / `ws` upstream endpoint schemes.
    pub allow_http_upstream: bool,
}

/// OAGW gear configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct OagwConfig {
    /// Total request timeout applied to non-streaming upstream calls.
    pub proxy_timeout_secs: u64,
    /// Upstream TCP/TLS connect timeout.
    pub connect_timeout_secs: u64,
    /// Idle timeout applied to streaming relays (SSE/WebSocket).
    pub idle_timeout_secs: u64,
    /// Hard request-body limit; larger payloads are rejected with 413 before
    /// buffering.
    pub max_body_size_bytes: u64,
    /// Plaintext upstream scheme policy.
    pub allow_http_upstream: bool,
    /// SSRF hardening switches.
    pub ssrf_policy: SsrfPolicy,
    /// OAuth2 client-credentials token cache TTL (ADR-0008).
    pub token_cache_ttl_secs: u64,
    /// OAuth2 client-credentials token cache capacity (ADR-0008).
    pub token_cache_capacity: usize,
    /// Maximum entries in the data-plane configuration cache (ADR-0005).
    pub dp_cache_capacity: usize,
    /// Maximum entries in the control-plane configuration cache.
    pub cp_cache_capacity: usize,
    /// When `true`, per-request plugin execution is traced at DEBUG level.
    pub trace_plugins: bool,
}

impl Default for OagwConfig {
    fn default() -> Self {
        Self {
            proxy_timeout_secs: DEFAULT_PROXY_TIMEOUT_SECS,
            connect_timeout_secs: DEFAULT_CONNECT_TIMEOUT_SECS,
            idle_timeout_secs: DEFAULT_IDLE_TIMEOUT_SECS,
            max_body_size_bytes: DEFAULT_BODY_LIMIT_BYTES,
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicy::default(),
            token_cache_ttl_secs: DEFAULT_TOKEN_CACHE_TTL_SECS,
            token_cache_capacity: DEFAULT_TOKEN_CACHE_CAPACITY,
            dp_cache_capacity: 1_000,
            cp_cache_capacity: 10_000,
            trace_plugins: false,
        }
    }
}

impl OagwConfig {
    /// Connect timeout as a [`std::time::Duration`].
    #[must_use]
    pub fn connect_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.connect_timeout_secs.max(1))
    }

    /// Proxy timeout as a [`std::time::Duration`].
    #[must_use]
    pub fn proxy_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.proxy_timeout_secs.max(1))
    }

    /// Idle timeout as a [`std::time::Duration`].
    #[must_use]
    pub fn idle_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.idle_timeout_secs.max(1))
    }

    /// `true` when plaintext upstream schemes (`http`, `ws`) may be dialled.
    #[must_use]
    pub const fn plaintext_upstreams_allowed(&self) -> bool {
        self.allow_http_upstream
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_follow_design_constraints() {
        let cfg = OagwConfig::default();
        assert_eq!(cfg.max_body_size_bytes, DEFAULT_BODY_LIMIT_BYTES);
        assert_eq!(cfg.token_cache_ttl_secs, DEFAULT_TOKEN_CACHE_TTL_SECS);
        assert_eq!(cfg.token_cache_capacity, DEFAULT_TOKEN_CACHE_CAPACITY);
        assert!(!cfg.allow_http_upstream);
    }

    #[test]
    fn deserializes_e2e_config_section() {
        let raw = serde_json::json!({
            "proxy_timeout_secs": 2,
            "allow_http_upstream": true,
            "ssrf_policy": {"enabled": false}
        });
        let cfg: OagwConfig = serde_json::from_value(raw).expect("parses");
        assert_eq!(cfg.proxy_timeout_secs, 2);
        assert!(cfg.allow_http_upstream);
        assert!(!cfg.ssrf_policy.enabled);
        assert_eq!(cfg.max_body_size_bytes, DEFAULT_BODY_LIMIT_BYTES);
    }

    #[test]
    fn timeouts_have_a_floor_of_one_second() {
        let cfg = OagwConfig {
            proxy_timeout_secs: 0,
            ..OagwConfig::default()
        };
        assert_eq!(cfg.proxy_timeout(), std::time::Duration::from_secs(1));
    }
}
